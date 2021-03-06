// Copyright 2014 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Local state for known hashes and their external location (blob reference).

use std::thunk::Thunk;
use std::time::duration::{Duration};
use rustc_serialize::hex::{ToHex};

use callback_container::{CallbackContainer};
use cumulative_counter::{CumulativeCounter};
use unique_priority_queue::{UniquePriorityQueue};
use process::{Process, MsgHandler};

use sqlite3::database::{Database};
use sqlite3::cursor::{Cursor};
use sqlite3::types::ResultCode::{SQLITE_DONE, SQLITE_OK, SQLITE_ROW};
use sqlite3::BindArg::{Integer64, Blob};
use sqlite3::{open};

use periodic_timer::{PeriodicTimer};

use sodiumoxide::crypto::hash::{sha512};


pub type HashIndexProcess = Process<Msg, Reply>;


/// A wrapper around Hash digests.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct Hash{
  pub bytes: Vec<u8>,
}

impl Hash {
  /// Computes `hash(text)` and stores this digest as the `bytes` field in a new `Hash` structure.
  pub fn new(text: &[u8]) -> Hash {
    let sha512::Digest(digest_bytes) = sha512::hash(text);
    Hash{bytes: digest_bytes[0 .. sha512::HASHBYTES].iter().map(|&x| x).collect()}
  }
}


/// An entry that can be inserted into the hash index.
#[derive(Clone)]
pub struct HashEntry{

  /// The hash of this entry (unique among all entries in the index).
  pub hash: Hash,

  /// The level in a hash tree that this entry is from. Level `0` represents `leaf`s, i.e. entries
  /// that represent user-data, where levels `1` and up represents `branches` of the tree,
  /// i.e. internal meta-data.
  pub level: i64,

  /// A local payload to store inside the index, along with this entry.
  pub payload: Option<Vec<u8>>,

  /// A reference to a location in the external persistent storage (a blob reference) that contains
  /// the data for this entry (e.g. an object-name and a byte range).
  pub persistent_ref: Option<Vec<u8>>,
}

pub enum Msg {
  /// Check whether this `Hash` already exists in the system.
  /// Returns `HashKnown` or `HashNotKnown`.
  HashExists(Hash),

  /// Locate the local payload of the `Hash`. This is currently not used.
  /// Returns `Payload` or `HashNotKnown`.
  FetchPayload(Hash),

  /// Locate the persistent reference (external blob reference) for this `Hash`.
  /// Returns `PersistentRef` or `HashNotKnown`.
  FetchPersistentRef(Hash),

  /// Reserve a `Hash` in the index, while sending its content to external storage.
  /// This is used to ensure that each `Hash` is stored only once.
  /// Returns `ReserveOK` or `HashKnown`.
  Reserve(HashEntry),

  /// Update the info for a reserved `Hash`. The `Hash` remains reserved. This is used to update
  /// the persistent reference (external blob reference) as soon as it is available (to allow new
  /// references to the `Hash` to be created before it is committed).
  /// Returns ReserveOK.
  UpdateReserved(HashEntry),

  /// A `Hash` is committed when it has been `finalized` in the external storage. `Commit` includes
  /// the persistent reference that the content is available at.
  /// Returns CommitOK.
  Commit(Hash, Vec<u8>),

  /// Install a "on-commit" handler to be called after `Hash` is committed.
  /// Returns `CallbackRegistered` or `HashNotKnown`.
  CallAfterHashIsComitted(Hash, Thunk<'static>),

  /// Flush the hash index to clear internal buffers and commit the underlying database.
  Flush,
}

pub enum Reply {
  HashKnown,
  HashNotKnown,
  Entry(HashEntry),

  Payload(Option<Vec<u8>>),
  PersistentRef(Vec<u8>),

  ReserveOK,
  CommitOK,
  CallbackRegistered,

  Retry,
}


#[derive(Clone)]
struct QueueEntry {
  id: i64,
  level: i64,
  payload: Option<Vec<u8>>,
  persistent_ref: Option<Vec<u8>>,
}

pub struct HashIndex {
  dbh: Database,

  id_counter: CumulativeCounter,

  queue: UniquePriorityQueue<i64, Vec<u8>, QueueEntry>,

  callbacks: CallbackContainer<Vec<u8>>,

  flush_timer: PeriodicTimer,

}

impl HashIndex {

  pub fn new(path: String) -> HashIndex {
    let mut hi = match open(&path) {
      Ok(dbh) => {
        HashIndex{dbh: dbh,
                  id_counter: CumulativeCounter::new(0),
                  queue: UniquePriorityQueue::new(),
                  callbacks: CallbackContainer::new(),
                  flush_timer: PeriodicTimer::new(Duration::seconds(10)),
        }
      },
      Err(err) => panic!("{:?}", err),
    };
    hi.exec_or_die("CREATE TABLE IF NOT EXISTS
                  hash_index (id        INTEGER PRIMARY KEY,
                              hash      BLOB,
                              height    INTEGER,
                              payload   BLOB,
                              blob_ref  BLOB)");

    hi.exec_or_die("CREATE UNIQUE INDEX IF NOT EXISTS
                  HashIndex_UniqueHash
                  ON hash_index(hash)");

    hi.exec_or_die("BEGIN");

    hi.refresh_id_counter();
    hi
  }

  #[cfg(test)]
  pub fn new_for_testing() -> HashIndex {
    HashIndex::new(":memory:".to_string())
  }

  fn exec_or_die(&mut self, sql: &str) {
    match self.dbh.exec(sql) {
      Ok(true) => (),
      Ok(false) => panic!("exec: {}", self.dbh.get_errmsg()),
      Err(msg) => panic!("exec: {:?}, {:?}\nIn sql: '{}'\n",
                         msg, self.dbh.get_errmsg(), sql)
    }
  }

  fn prepare_or_die<'a>(&'a self, sql: &str) -> Cursor<'a> {
    match self.dbh.prepare(sql, &None) {
      Ok(s)  => s,
      Err(x) => panic!("sqlite error: {} ({:?})",
                       self.dbh.get_errmsg(), x),
    }
  }

  fn select1<'a>(&'a mut self, sql: &str) -> Option<Cursor<'a>> {
    let mut cursor = self.prepare_or_die(sql);
    if cursor.step() == SQLITE_ROW { Some(cursor) } else { None }
  }

  fn index_locate(&mut self, hash: &Hash) -> Option<QueueEntry> {
    assert!(hash.bytes.len() > 0);

    let result_opt = self.select1(&format!(
      "SELECT id, height, payload, blob_ref FROM hash_index WHERE hash=x'{}'",
      hash.bytes.to_hex()
    ));
    result_opt.map(|result| {
      let mut result = result;
      let id = result.get_int(0) as i64;
      let level = result.get_int(1) as i64;
      let payload: Vec<u8> = result.get_blob(2).unwrap_or(&[]).iter().map(|&x| x).collect();
      let persistent_ref: Vec<u8> = result.get_blob(3).unwrap_or(&[]).iter().map(|&x| x).collect();
      QueueEntry{id: id, level: level,
                 payload: if payload.len() == 0 { None }
                          else {Some(payload) },
                 persistent_ref: Some(persistent_ref)
      } })
  }

  fn locate(&mut self, hash: &Hash) -> Option<QueueEntry> {
    let result_opt = self.queue.find_value_of_key(&hash.bytes);
    result_opt.map(|x| x).or_else(|| self.index_locate(hash))
  }

  fn refresh_id_counter(&mut self) {
    let id = self.select1("SELECT MAX(id) FROM hash_index").expect("id").get_int(0);
    self.id_counter = CumulativeCounter::new(id as i64);
  }

  fn next_id(&mut self) -> i64 {
    self.id_counter.next()
  }

  fn reserve(&mut self, hash_entry: HashEntry) -> i64 {
    self.maybe_flush();

    let HashEntry{hash, level, payload, persistent_ref} = hash_entry;
    assert!(hash.bytes.len() > 0);

    let my_id = self.next_id();

    assert!(self.queue.reserve_priority(my_id, hash.bytes.clone()).is_ok());
    self.queue.put_value(hash.bytes,
                         QueueEntry{id: my_id,
                                    level: level,
                                    payload: payload,
                                    persistent_ref: persistent_ref
                         });
    my_id
  }

  fn update_reserved(&mut self, hash_entry: HashEntry) {
    let HashEntry{hash, level, payload, persistent_ref} = hash_entry;
    assert!(hash.bytes.len() > 0);
    let old_entry = self.locate(&hash).expect("hash was reserved");

    // If we didn't already commit and pop() the hash, update it:
    let id_opt = self.queue.find_key(&hash.bytes).map(|id| id.clone());
    if id_opt.is_some() {
      assert_eq!(id_opt, Some(old_entry.id));
      self.queue.update_value(&hash.bytes,
                              |qe| QueueEntry{level: level,
                                              payload: payload.clone(),
                                              persistent_ref: persistent_ref.clone(),
                                              ..qe.clone()});
    }
  }

  fn register_hash_callback(&mut self, hash: &Hash, callback: Thunk<'static>) -> bool {
    assert!(hash.bytes.len() > 0);

    if self.queue.find_value_of_key(&hash.bytes).is_some() {
      self.callbacks.add(hash.bytes.clone(), callback);
    } else if self.locate(hash).is_some() {
      // Hash was already committed
      callback();
    } else {
      // We cannot register this callback, since the hash doesn't exist anywhere
      return false
    }

    return true;
  }

  fn insert_completed_in_order(&mut self) {
    let mut insert_stm = self.dbh.prepare(
      "INSERT INTO hash_index (id, hash, height, payload, blob_ref) VALUES (?, ?, ?, ?, ?)",
      &None).unwrap();

    loop {
      match self.queue.pop_min_if_complete() {
        None => break,
        Some((id, hash_bytes, queue_entry)) => {
          assert_eq!(id, queue_entry.id);

          let child_refs_opt = queue_entry.payload;
          let payload = child_refs_opt.unwrap_or_else(|| vec!());
          let level = queue_entry.level;
          let persistent_ref = queue_entry.persistent_ref.expect("hash was comitted");

          assert_eq!(SQLITE_OK, insert_stm.bind_param(1, &Integer64(id)));
          assert_eq!(SQLITE_OK, insert_stm.bind_param(2, &Blob(hash_bytes.clone())));
          assert_eq!(SQLITE_OK, insert_stm.bind_param(3, &Integer64(level)));
          assert_eq!(SQLITE_OK, insert_stm.bind_param(4, &Blob(payload)));
          assert_eq!(SQLITE_OK, insert_stm.bind_param(5, &Blob(persistent_ref)));

          assert_eq!(SQLITE_DONE, insert_stm.step());

          assert_eq!(SQLITE_OK, insert_stm.clear_bindings());
          assert_eq!(SQLITE_OK, insert_stm.reset());

          self.callbacks.allow_flush_of(&hash_bytes);
        },
      }
    }
  }

  fn commit(&mut self, hash: &Hash, blob_ref: &Vec<u8>) {
    // Update persistent reference for ready hash
    let queue_entry = self.locate(hash).expect("hash was committed");
    self.queue.update_value(&hash.bytes,
                            |old_qe| QueueEntry{persistent_ref: Some(blob_ref.clone()),
                                                ..old_qe.clone()});
    self.queue.set_ready(queue_entry.id);

    self.insert_completed_in_order();

    self.maybe_flush();
  }

  fn maybe_flush(&mut self) {
    if self.flush_timer.did_fire() {
      self.flush();
    }
  }

  fn flush(&mut self) {
    // Callbacks assume their data is safe, so commit before calling them
    self.exec_or_die("COMMIT; BEGIN");

    // Run ready callbacks
    self.callbacks.flush();
  }
}

// #[unsafe_desctructor]
// impl  Drop for HashIndex {
//   fn drop(&mut self) {
//     self.flush();

//     assert_eq!(self.callbacks.len(), 0);

//     assert_eq!(self.queue.len(), 0);
//     self.exec_or_die("COMMIT");
//   }
// }


impl MsgHandler<Msg, Reply> for HashIndex {
  fn handle(&mut self, msg: Msg, reply: Box<Fn(Reply)>) {
    match msg {

      Msg::HashExists(hash) => {
        assert!(hash.bytes.len() > 0);
        return reply(match self.locate(&hash) {
          Some(_) => Reply::HashKnown,
          None => Reply::HashNotKnown,
        });
      },

      Msg::FetchPayload(hash) => {
        assert!(hash.bytes.len() > 0);
        return reply(match self.locate(&hash) {
          Some(ref queue_entry) => Reply::Payload(queue_entry.payload.clone()),
          None => Reply::HashNotKnown,
        });
      },

      Msg::FetchPersistentRef(hash) => {
        assert!(hash.bytes.len() > 0);
        return reply(match self.locate(&hash) {
          Some(ref queue_entry) if queue_entry.persistent_ref.is_none() => Reply::Retry,
          Some(queue_entry) =>
            Reply::PersistentRef(queue_entry.persistent_ref.expect("persistent_ref")),
          None => Reply::HashNotKnown,
        });
      },

      Msg::Reserve(hash_entry) => {
        assert!(hash_entry.hash.bytes.len() > 0);
        // To avoid unused IO, we store entries in-memory until committed to persistent storage.
        // This allows us to continue after a crash without needing to scan through and delete
        // uncommitted entries.
        return reply(match self.locate(&hash_entry.hash) {
          Some(_) => Reply::HashKnown,
          None => { self.reserve(hash_entry); Reply::ReserveOK },
        });
      },

      Msg::UpdateReserved(hash_entry) => {
        assert!(hash_entry.hash.bytes.len() > 0);
        self.update_reserved(hash_entry);
        return reply(Reply::ReserveOK);
      }

      Msg::Commit(hash, persistent_ref) => {
        assert!(hash.bytes.len() > 0);
        self.commit(&hash, &persistent_ref);
        return reply(Reply::CommitOK);
      },

      Msg::CallAfterHashIsComitted(hash, callback) => {
        assert!(hash.bytes.len() > 0);
        if self.register_hash_callback(&hash, callback) {
          return reply(Reply::CallbackRegistered);
        } else {
          return reply(Reply::HashNotKnown);
        }
      },

      Msg::Flush => {
        self.flush();
        return reply(Reply::CommitOK);
      }
    }
  }
}
