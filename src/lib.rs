extern crate bincode;
extern crate rustc_serialize;
extern crate rand;

use bincode::SizeLimit;
use bincode::rustc_serialize::{encode, decode};
use rustc_serialize::{Encodable, Decodable};

use std::cmp::max;
use std::convert::From;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Read, Write, Seek, SeekFrom, ErrorKind};
use std::mem::{size_of};
use std::str;

const NUM_CHILDREN: usize = 32;
const FILE_HEADER: &'static str = "B+Tree\0";
const CURRENT_VERSION: u8 = 0x01;

// specify the types for the keys & values
pub trait KeyType: Ord + Encodable + Decodable {}
pub trait ValueType: Ord + Encodable + Decodable {}

// provide generic implementations
impl<T> KeyType for T where T: Ord + Encodable + Decodable {}
impl<T> ValueType for T where T: Ord + Encodable + Decodable {}

#[derive(RustcEncodable, RustcDecodable, PartialEq)]
enum Payload<K: KeyType, V: ValueType> {
        Value(V),
        Children([(K,u64); NUM_CHILDREN]),
    }

#[derive(RustcEncodable, RustcDecodable, PartialEq)]
struct Node<K: KeyType, V: ValueType> {
    key: K,
    parent: u64,
    payload: Payload<K,V>, // either children, or actual values
}

#[derive(RustcEncodable, RustcDecodable, PartialEq)]
struct WALRecord<K: KeyType, V: ValueType> {
    key: K,
    value: V,
}

/// This struct represents an on-disk B+Tree. There are NUM_CHILDREN keys at each
/// level in the tree. The on-disk format is as follows where VV is the version
/// number:
/// |-------------------------------------------|
/// | 0x42 0x2b 0x54 0x72 | 0x65 0x65 0x00 0xVV |
/// | B    +    T    r    | e    e    \0   0xVV |
/// |-------------------------------------------|
/// | smallest record in bincode format         |
/// |-------------------------------------------|
/// | ...                                       |
/// |-------------------------------------------|
/// | largest record in bincode format          |
/// |-------------------------------------------|
/// | internal nodes ...                        |
/// |-------------------------------------------|
/// | root node                                 |
/// |-------------------------------------------|
pub struct BTree<K: KeyType, V: ValueType> {
    tree_file: File,                // the file backing the whole thing
    wal_file: File,                 // write-ahead log for in-memory items
    root: Option<Node<K,V>>,        // optional in-memory copy of the root node
    max_key_size: usize,            // the size of the key in bytes
    max_value_size: usize,          // the size of the value in bytes
    mem_tree: BTreeMap<K, BTreeSet<V>>,  // the in-memory BTree that gets merged with the on-disk one
}

impl <K: KeyType, V: ValueType> BTree<K, V> {
    pub fn new(tree_file_path: String, max_key_size: usize, max_value_size: usize) -> Result<BTree<K,V>, Box<Error>> {
        // create our mem_tree
        let mut mem_tree = BTreeMap::<K, BTreeSet<V>>::new();

        let mut wal_file = try!(OpenOptions::new().read(true).write(true).create(true).open(tree_file_path.to_owned() + ".wal"));

        let record_size = max_key_size + max_value_size;

        // if we have a WAL file, replay it into the mem_tree
        if try!(wal_file.metadata()).len() != 0 {
            let mut buff = vec![0; record_size];

            loop {
                match wal_file.read_exact(&mut buff) {
                    Ok(_) => {
                        let record: WALRecord<K,V> = try!(decode(&buff));  // decode the record
                        mem_tree.entry(record.key).or_insert(BTreeSet::<V>::new()).insert(record.value);  // add it to the in-memory table
                    },
                    Err(e) => if e.kind() == ErrorKind::UnexpectedEof {
                        break  // reached the end of our file, break from the loop
                    } else {
                        return Err(From::from(e));
                    }
                }
            }
        }

        // compute the size of a on-disk Node
        let node_size: usize = (max_key_size + size_of::<u64>() + max(max_value_size, (max_key_size + size_of::<u64>()) * NUM_CHILDREN)) as usize;

        // open the data file
        let mut tree_file = try!(OpenOptions::new().read(true).write(true).create(true).open(tree_file_path));

        let metadata = try!(tree_file.metadata());

        // check to see if this is a new file
        if metadata.len() == 0 {
            // write out our header
            try!(tree_file.write(FILE_HEADER.as_bytes()));
            
            // write out our version
            try!(tree_file.write(&[CURRENT_VERSION]));

            // construct and return our BTree object
            Ok(BTree{tree_file: tree_file,
                     wal_file: wal_file,
                     root: None,
                     max_key_size: max_key_size,
                     max_value_size: max_value_size,
                     mem_tree: mem_tree
            })
        } else {
            let mut version_string = vec![0; 8];

            try!(tree_file.read_exact(&mut version_string));

            // make sure we've opened a proper file
            if try!(str::from_utf8(&version_string[0..FILE_HEADER.len()])) != FILE_HEADER ||
               version_string[FILE_HEADER.len()] != CURRENT_VERSION {
                return Err(From::from(std::io::Error::new(ErrorKind::InvalidData, "Invalid BTree file or BTree version")));
            }

            let mut buff = vec![0; node_size];

            // make sure we have a root node to read
            if metadata.len() < (version_string.len() + node_size) as u64 {
                // if we don't have a root node yet, just return
                return Ok(BTree{tree_file: tree_file,
                                wal_file: wal_file,
                                root: None,
                                max_key_size: max_key_size,
                                max_value_size: max_value_size,
                                mem_tree: mem_tree
                });
            }
            
            // seek node_size in from the end of the file to read the root node
            try!(tree_file.seek(SeekFrom::End((node_size as isize * -1) as i64)));
            try!(tree_file.read_exact(&mut buff));

            let root_node: Node<K,V> = try!(decode(&buff[..]));

            Ok(BTree{tree_file: tree_file,
                     wal_file: wal_file,
                     root: Some(root_node),
                     max_key_size: max_key_size,
                     max_value_size: max_value_size,
                     mem_tree: mem_tree
            })
        }
    }

    /// Inserts a key into the BTree
    pub fn insert(&mut self, key: K, value: V) -> Result<usize, Box<Error>> {
        let record = WALRecord{key: key, value: value};

        // encode the record
        let record_size = self.max_key_size + self.max_value_size;
        let mut buff = try!(encode(&record, SizeLimit::Bounded(record_size as u64)));

        // padd it out to the max size
        if buff.len() > self.max_key_size + self.max_value_size {
            return Err(From::from(std::io::Error::new(ErrorKind::InvalidData, "Key and value size are too large")));
        } else {
            let diff = (self.max_key_size + self.max_value_size) - buff.len();
            buff.extend(vec![0; diff]);
        }

        try!(self.wal_file.write_all(&buff));

        let WALRecord{key, value} = record;

        self.mem_tree.entry(key).or_insert(BTreeSet::<V>::new()).insert(value);

        Ok(buff.len())
    }

    /// Merges the records on disk with the records in memory
    fn compact(&mut self) {

    }
}


#[cfg(test)]
mod tests {

    use std::fs;
    use std::fs::OpenOptions;
    use ::BTree;
    use rand::{thread_rng, Rng};


    fn gen_temp_name() -> String {
        let file_name: String = thread_rng().gen_ascii_chars().take(10).collect();

        return "/tmp/".to_owned() + file_name.as_str() + ".btr";
    }

    fn remove_files(file_path: String) {
        fs::remove_file(&file_path);
        fs::remove_file(file_path + ".wal");
    }

    #[test]
    fn new_blank_file() {
        let file_path = gen_temp_name();

        BTree::<u8, u8>::new(file_path.to_owned(), 1, 1).unwrap();

        // make sure our two files were created
        let btf = OpenOptions::new().read(true).write(false).create(false).open(&file_path).unwrap();
        assert!(btf.metadata().unwrap().len() == 8);

        let wal = OpenOptions::new().read(true).write(false).create(false).open(file_path.to_owned() + ".wal").unwrap();
        assert!(wal.metadata().unwrap().len() == 0);

        remove_files(file_path); // remove files assuming it all went well
    }

    #[test]
    fn new_existing_file() {
        let file_path = gen_temp_name();

        {
            BTree::<u8, u8>::new(file_path.to_owned(), 1, 1).unwrap();
        }

        let btree = BTree::<u8, u8>::new(file_path.to_owned(), 1, 1).unwrap();

        // check our file lengths from the struct
        assert!(btree.tree_file.metadata().unwrap().len() == 8);
        assert!(btree.wal_file.metadata().unwrap().len() == 0);

        remove_files(file_path); // remove files assuming it all went well
    }

    #[test]
    fn insert_new_u8() {
        let file_path = gen_temp_name();

        let mut btree = BTree::<u8, u8>::new(file_path.to_owned(), 1, 1).unwrap();

        let len = btree.insert(2, 3).unwrap(); // insert into a new file

        println!("LENGTH: {}", len);

        assert!(btree.wal_file.metadata().unwrap().len() == 2);

        remove_files(file_path); // remove files assuming it all went well
    }

    #[test]
    fn insert_new_str() {
        let file_path = gen_temp_name();

        let mut btree = BTree::<String, String>::new(file_path.to_owned(), 15, 15).unwrap();

        let size = btree.insert("Hello".to_owned(), "World".to_owned()).unwrap(); // insert into a new file

        assert!(btree.wal_file.metadata().unwrap().len() == size as u64);

        remove_files(file_path); // remove files assuming it all went well
    }
}
