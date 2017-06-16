use std::collections::BTreeMap;
use std::cmp::Ordering;
use std::default::Default;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use digest::Digest;
use ignore::WalkBuilder;
use time;

use serde_bytes;
use serde_cbor;
use serde_json;

use sha2;
use sha3;

use error;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Database(Entry);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Entry {
    Directory(BTreeMap<PathBuf, Entry>),
    File(Metrics),
}

impl Default for Entry {
    fn default() -> Entry {
        Entry::Directory(BTreeMap::default())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    sha2: HashSum,
    sha3: HashSum,
    size: u64,      // File size
    nul: bool,      // Does the file contain a NUL byte?
    nonascii: bool, // Does the file contain non-ASCII bytes?
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashSum(#[serde(with = "serde_bytes")] Vec<u8>);

#[derive(Default)]
struct EngineSize(u64);
impl EngineSize {
    fn input(&mut self, input: &[u8]) {
        self.0 += input.len() as u64;
    }
    fn result(self) -> u64 {
        self.0
    }
}

#[derive(Default)]
struct EngineNul(bool);
impl EngineNul {
    fn input(&mut self, input: &[u8]) {
        self.0 = self.0 || input.iter().any(|x| *x == 0);
    }
    fn result(self) -> bool {
        self.0
    }
}

#[derive(Default)]
struct EngineNonascii(bool);
impl EngineNonascii {
    fn input(&mut self, input: &[u8]) {
        self.0 = self.0 || input.iter().any(|x| x & 0x80 != 0);
    }
    fn result(self) -> bool {
        self.0
    }
}

#[derive(Default)]
struct Engines {
    sha2: sha2::Sha256,
    sha3: sha3::Sha3_256,
    size: EngineSize,
    nul: EngineNul,
    nonascii: EngineNonascii,
}

impl Engines {
    fn input(&mut self, input: &[u8]) {
        self.sha2.input(input);
        self.sha3.input(input);
        self.size.input(input);
        self.nul.input(input);
        self.nonascii.input(input);
    }
    fn result(self) -> Metrics {
        Metrics {
            sha2: HashSum(Vec::from(self.sha2.result().as_slice())),
            sha3: HashSum(Vec::from(self.sha3.result().as_slice())),
            size: self.size.result(),
            nul: self.nul.result(),
            nonascii: self.nonascii.result(),
        }
    }
}

fn compute_metrics<P>(path: P) -> Result<Metrics, error::Error>
where
    P: AsRef<Path>
{
    let mut f = File::open(path)?;

    let mut engines = Engines::default();

    let mut buffer = [0; 4096];
    loop {
        let n = f.read(&mut buffer[..])?;
        if n == 0 { break }
        engines.input(&buffer[0..n]);
    }
    Ok(engines.result())
}

trait BTreeMapExt<K, V> where K: Ord, V: Default {
    fn get_default(&mut self, key: K) -> &mut V;
}

impl<K, V> BTreeMapExt<K, V> for BTreeMap<K, V>
where
    K: Ord + Clone,
    V: Default,
{
    fn get_default(&mut self, key: K) -> &mut V {
        self.entry(key).or_insert_with(|| V::default())
    }
}

impl Entry {
    fn insert(&mut self, path: PathBuf, file: Entry) {
        // Inner nodes in the tree should always be directories. If
        // the node is not a directory, that means we are inserting a
        // duplicate file. However, this function is only called from
        // the directory walker, which makes it impossible to observe
        // any duplicates. (And the database, after construction, is
        // always immutable.)
        match self {
            &mut Entry::Directory(ref mut entries) => {
                let mut components = path.components();
                let count = components.clone().count();
                let first = Path::new(components.next().expect("unreachable").as_os_str()).to_owned();
                let rest = components.as_path().to_owned();
                if count > 1 {
                    let mut subentry = entries.get_default(first);
                    subentry.insert(rest, file);
                } else {
                    match entries.insert(first, file) {
                        Some(_) => unreachable!(), // See above
                        None => (),
                    }
                }
            }
            &mut Entry::File(_) => unreachable!()
        }
    }

    fn lookup(&self, path: &PathBuf) -> Option<&Entry> {
        match *self {
            Entry::Directory(ref entries) => {
                let mut components = path.components();
                let count = components.clone().count();
                let first = Path::new(components.next().expect("unreachable").as_os_str()).to_owned();
                let rest = components.as_path().to_owned();
                if count > 1 {
                    entries.get(&first).and_then(
                        |subentry| subentry.lookup(&rest))
                } else {
                    entries.get(&first)
                }
            }
            Entry::File(_) => unreachable!()
        }
    }
}

#[derive(Debug)]
pub enum EntryDiff {
    Directory(BTreeMap<PathBuf, EntryDiff>, DirectoryDiff),
    File(MetricsDiff),
    KindChanged,
}

#[derive(Debug)]
pub struct DirectoryDiff {
    added: u64,
    removed: u64,
    changed: u64,
    unchanged: u64,
}

#[derive(Debug)]
pub struct MetricsDiff {
    changed_content: bool,
    zeroed: bool,
    changed_nul: bool,
    changed_nonascii: bool,
}

impl EntryDiff {
    fn show_diff(&self, path: &PathBuf, depth: usize) {
        match *self {
            EntryDiff::Directory(ref entries, ref diff) => {
                if diff.changed > 0 || diff.added > 0 || diff.removed > 0 {
                    println!("{}{}: {} changed, {} added, {} removed, {} unchanged",
                             "| ".repeat(depth),
                             path.display(),
                             diff.changed,
                             diff.added,
                             diff.removed,
                             diff.unchanged);
                    for (key, entry) in entries.iter() {
                        entry.show_diff(key, depth+1);
                    }
                }
            }
            EntryDiff::File(ref diff) => {
                if diff.zeroed || diff.changed_nul || diff.changed_nonascii {
                    println!("{}{} changed",
                             "| ".repeat(depth),
                             path.display());
                    if diff.zeroed {
                        println!("{}> suspicious: file was truncated",
                                 "##".repeat(depth));
                    }
                    if diff.changed_nul {
                        println!("{}> suspicious: original had no NUL bytes, but now does",
                                 "##".repeat(depth));
                    }
                    if diff.changed_nonascii {
                        println!("{}> suspicious: original had no non-ASCII bytes, but now does",
                                 "##".repeat(depth));
                    }
                }
            }
            EntryDiff::KindChanged => {
            }
        }
    }
}

impl Entry {
    fn diff(&self, other: &Entry) -> EntryDiff {
        match (self, other) {
            (&Entry::Directory(ref old), &Entry::Directory(ref new)) => {
                let mut entries = BTreeMap::default();
                let mut added = 0;
                let mut removed = 0;
                let mut changed = 0;
                let mut unchanged = 0;

                let mut old_iter = old.iter();
                let mut new_iter = new.iter();
                let mut old_entry = old_iter.next();
                let mut new_entry = new_iter.next();
                while old_entry.is_some() && new_entry.is_some() {
                    let (old_key, old_value) = old_entry.unwrap();
                    let (new_key, new_value) = new_entry.unwrap();
                    match old_key.cmp(new_key) {
                        Ordering::Less => {
                            removed += 1;
                            old_entry = old_iter.next();
                        }
                        Ordering::Greater => {
                            added += 1;
                            new_entry = new_iter.next();
                        }
                        Ordering::Equal => {
                            let diff = old_value.diff(new_value);
                            match diff {
                                EntryDiff::Directory(_, ref stats) => {
                                    added += stats.added;
                                    removed += stats.removed;
                                    changed += stats.changed;
                                    unchanged += stats.unchanged;
                                }
                                EntryDiff::File(ref stats) => {
                                    if stats.changed_content {
                                        changed += 1;
                                    } else {
                                        unchanged += 1;
                                    }
                                }
                                EntryDiff::KindChanged => {
                                    changed += 1;
                                }
                            }
                            entries.insert(old_key.clone(), diff);
                            old_entry = old_iter.next();
                            new_entry = new_iter.next();
                        }
                    }
                }
                removed += old_iter.count() as u64;
                added += new_iter.count() as u64;
                EntryDiff::Directory(
                    entries,
                    DirectoryDiff { added, removed, changed, unchanged })
            },
            (&Entry::File(ref old), &Entry::File(ref new)) =>
                EntryDiff::File(
                    MetricsDiff {
                        changed_content: old.size != new.size ||
                            old.sha2 != new.sha2 ||
                            old.sha3 != new.sha3,
                        zeroed: old.size > 0 && new.size == 0,
                        changed_nul: old.nul != new.nul,
                        changed_nonascii: old.nonascii != new.nonascii,
                    }
                ),
            (_, _) => EntryDiff::KindChanged,
        }
    }
}

impl Database {
    fn insert(&mut self, path: PathBuf, entry: Entry) {
        self.0.insert(path, entry);
    }

    pub fn lookup(&self, path: &PathBuf) -> Option<&Entry> {
        self.0.lookup(path)
    }

    pub fn diff(&self, other: &Database) -> EntryDiff {
        self.0.diff(&other.0)
    }

    pub fn build<P>(root: P, verbose: bool) -> Result<Database, error::Error>
    where
        P: AsRef<Path>,
    {
        let mut total_bytes = 0;
        let start_time_ns = time::precise_time_ns();
        let mut database = Database::default();
        for entry in WalkBuilder::new(&root).build() {
            let entry = entry?;
            if entry.file_type().map_or(false, |t| t.is_file()) {
                let metrics = compute_metrics(entry.path())?;
                total_bytes += metrics.size;
                let result = Entry::File(metrics);
                let short_path = if entry.path() == root.as_ref() {
                    Path::new(entry.path().file_name().expect("unreachable"))
                } else {
                    entry.path().strip_prefix(&root)?
                };
                database.insert(short_path.to_owned(), result);
            }
        }
        let stop_time_ns = time::precise_time_ns();
        if verbose {
            println!("Database::build took {:.3} seconds, read {} bytes, {:.1} MB/s",
                     (stop_time_ns - start_time_ns) as f64/1e9,
                     total_bytes,
                     total_bytes as f64/((stop_time_ns - start_time_ns) as f64/1e3));
        }
        Ok(database)
    }

    pub fn show_diff(&self, other: &Database) {
        let diff = self.diff(other);
        diff.show_diff(&Path::new(".").to_owned(), 0);
    }

    pub fn check<P>(&self, root: P) -> Result<(), error::Error>
    where
        P: AsRef<Path>,
    {
        // FIXME: This is non-interactive, but vastly simply than
        // trying to implement the same functionality interactively.
        let other = Database::build(root, false)?;
        self.show_diff(&other);
        Ok(())
    }

    pub fn load_json<P>(path: P) -> Result<Database, error::Error>
    where
        P: AsRef<Path>
    {
        let f = File::open(path)?;
        Ok(serde_json::from_reader(f)?)
    }

    pub fn dump_json<P>(&self, path: P) -> Result<(), error::Error>
    where
        P: AsRef<Path>
    {
        let json = serde_json::to_string(self)?;
        let mut f = File::create(path)?;
        write!(f, "{}", json)?;
        Ok(())
    }

    pub fn load_cbor<P>(path: P) -> Result<Database, error::Error>
    where
        P: AsRef<Path>
    {
        let f = File::open(path)?;
        Ok(serde_cbor::from_reader(f)?)
    }

    pub fn dump_cbor<P>(&self, path: P) -> Result<(), error::Error>
    where
        P: AsRef<Path>
    {
        let cbor = serde_cbor::to_vec(self)?;
        let mut f = File::create(path)?;
        f.write_all(cbor.as_slice())?;
        Ok(())
    }
}

// impl std::fmt::Display for Database {
//     fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
//         for (path, entry) in self.0.iter() {
//             match entry {
//                 &Entry::File(ref hashes) => {
//                     let hash: Vec<_> = hashes.sha2.0.iter().map(
//                         |b| format!("{:02x}", b)).collect();
//                     writeln!(f, "{} {}", hash.join(""), Path::new(path).display())?
//                 }
//             }
//         }
//         Ok(())
//     }
// }