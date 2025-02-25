use automerge_repo::{DocumentId, Storage, StorageError};
use futures::future::TryFutureExt;
use futures::Future;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::marker::Unpin;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::oneshot::{channel as oneshot, Sender as OneShot};

pub struct SimpleStorage;

impl Storage for SimpleStorage {}

#[derive(Clone, Debug, Default)]
pub struct InMemoryStorage {
    documents: Arc<Mutex<HashMap<DocumentId, Vec<u8>>>>,
}

impl InMemoryStorage {
    pub fn add_document(&self, doc_id: DocumentId, mut doc: Vec<u8>) {
        let mut documents = self.documents.lock();
        let entry = documents.entry(doc_id).or_insert_with(Default::default);
        entry.append(&mut doc);
    }
    
    pub fn contains_document(&self, doc_id: DocumentId) -> bool {
        self.documents.lock().contains_key(&doc_id)
    }
}

impl Storage for InMemoryStorage {
    fn get(
        &self,
        id: DocumentId,
    ) -> Box<dyn Future<Output = Result<Option<Vec<u8>>, StorageError>> + Send + Unpin> {
        Box::new(futures::future::ready(Ok(self
            .documents
            .lock()
            .get(&id)
            .cloned())))
    }

    fn list_all(
        &self,
    ) -> Box<dyn Future<Output = Result<Vec<DocumentId>, StorageError>> + Send + Unpin> {
        Box::new(futures::future::ready(Ok(self
            .documents
            .lock()
            .keys()
            .cloned()
            .collect())))
    }

    fn append(
        &self,
        id: DocumentId,
        mut changes: Vec<u8>,
    ) -> Box<dyn Future<Output = Result<(), StorageError>> + Send + Unpin> {
        let mut documents = self.documents.lock();
        let entry = documents.entry(id).or_insert_with(Default::default);
        entry.append(&mut changes);
        Box::new(futures::future::ready(Ok(())))
    }

    fn compact(
        &self,
        id: DocumentId,
        full_doc: Vec<u8>,
    ) -> Box<dyn Future<Output = Result<(), StorageError>> + Send + Unpin> {
        let mut documents = self.documents.lock();
        documents.insert(id, full_doc);
        Box::new(futures::future::ready(Ok(())))
    }
}

#[derive(Debug)]
enum StorageRequest {
    Load(DocumentId, OneShot<Option<Vec<u8>>>),
    Append(DocumentId, Vec<u8>, OneShot<()>),
    Compact(DocumentId, Vec<u8>, OneShot<()>),
    ListAll(OneShot<Vec<DocumentId>>),
    ProcessNextResult,
}

#[derive(Clone, Debug)]
pub struct AsyncInMemoryStorage {
    chan: Sender<StorageRequest>,
}

impl AsyncInMemoryStorage {
    pub fn new(mut documents: HashMap<DocumentId, Vec<u8>>, with_step: bool) -> Self {
        let (doc_request_sender, mut doc_request_receiver) = channel::<StorageRequest>(1);
        let mut results = VecDeque::new();
        let mut can_send_result = false;
        tokio::spawn(async move {
            while let Some(request) = doc_request_receiver.recv().await {
                match request {
                    StorageRequest::ListAll(sender) => {
                        let result = documents.keys().cloned().collect();
                        let (tx, rx) = oneshot();
                        results.push_back(tx);
                        tokio::spawn(async move {
                            rx.await.unwrap();
                            let _ = sender.send(result);
                        });
                    }
                    StorageRequest::Load(doc_id, sender) => {
                        let result = documents.get(&doc_id).cloned();
                        let (tx, rx) = oneshot();
                        results.push_back(tx);
                        tokio::spawn(async move {
                            rx.await.unwrap();
                            let _ = sender.send(result);
                        });
                    }
                    StorageRequest::Append(doc_id, mut data, sender) => {
                        let entry = documents.entry(doc_id).or_insert_with(Default::default);
                        entry.append(&mut data);
                        let (tx, rx) = oneshot();
                        results.push_back(tx);
                        tokio::spawn(async move {
                            rx.await.unwrap();
                            let _ = sender.send(());
                        });
                    }
                    StorageRequest::Compact(doc_id, data, sender) => {
                        let _entry = documents
                            .entry(doc_id)
                            .and_modify(|entry| *entry = data)
                            .or_insert_with(Default::default);
                        let (tx, rx) = oneshot();
                        results.push_back(tx);
                        tokio::spawn(async move {
                            rx.await.unwrap();
                            let _ = sender.send(());
                        });
                    }
                    StorageRequest::ProcessNextResult => {
                        if let Some(sender) = results.pop_front() {
                            let _ = sender.send(());
                        } else {
                            can_send_result = true;
                        }
                    }
                }
                if !with_step || can_send_result {
                    let sender: OneShot<()> = results.pop_front().unwrap();
                    let _ = sender.send(());
                }
            }
        });
        AsyncInMemoryStorage {
            chan: doc_request_sender,
        }
    }

    pub async fn process_next_result(&self) {
        self.chan
            .send(StorageRequest::ProcessNextResult)
            .await
            .unwrap();
    }
}

impl Storage for AsyncInMemoryStorage {
    fn get(
        &self,
        id: DocumentId,
    ) -> Box<dyn Future<Output = Result<Option<Vec<u8>>, StorageError>> + Send + Unpin> {
        let (tx, rx) = oneshot();
        self.chan
            .blocking_send(StorageRequest::Load(id, tx))
            .unwrap();
        Box::new(rx.map_err(|_| StorageError::Error))
    }

    fn list_all(
        &self,
    ) -> Box<dyn Future<Output = Result<Vec<DocumentId>, StorageError>> + Send + Unpin> {
        let (tx, rx) = oneshot();
        self.chan
            .blocking_send(StorageRequest::ListAll(tx))
            .unwrap();
        Box::new(rx.map_err(|_| StorageError::Error))
    }

    fn append(
        &self,
        id: DocumentId,
        changes: Vec<u8>,
    ) -> Box<dyn Future<Output = Result<(), StorageError>> + Send + Unpin> {
        let (tx, rx) = oneshot();
        self.chan
            .blocking_send(StorageRequest::Append(id, changes, tx))
            .unwrap();
        Box::new(rx.map_err(|_| StorageError::Error))
    }

    fn compact(
        &self,
        id: DocumentId,
        full_doc: Vec<u8>,
    ) -> Box<dyn Future<Output = Result<(), StorageError>> + Send + Unpin> {
        let (tx, rx) = oneshot();
        self.chan
            .blocking_send(StorageRequest::Compact(id, full_doc, tx))
            .unwrap();
        Box::new(rx.map_err(|_| StorageError::Error))
    }
}
