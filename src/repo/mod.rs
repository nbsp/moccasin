use crate::config::{Config, SortOrder};
use crate::feed::Feed;
use anyhow::Result;
use polodb_core::{bson, bson::doc, Database};
use std::fmt::Debug;
use tokio::sync::mpsc::{self, UnboundedSender};

#[derive(Clone, Debug)]
pub enum StorageEvent {
    RetrievedAll(Vec<Feed>),
    Requesting(usize),
    Fetched((usize, usize)),
}

#[derive(Debug)]
enum FetchErr {
    Request,
    Deserialize,
    Parse,
}

pub struct Repository {
    db: Database,
    app_tx: mpsc::UnboundedSender<StorageEvent>,
    db_tx: mpsc::UnboundedSender<StorageEvent>,
    db_rx: mpsc::UnboundedReceiver<StorageEvent>,
}

impl Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Database {}")
    }
}

fn sort_feeds(feeds: &mut Vec<Feed>, config: &Config) {
    match config.sort_order() {
        SortOrder::Az => {
            feeds.sort_by(|a, b| a.title().partial_cmp(b.title()).unwrap());
        }
        SortOrder::Za => {
            feeds.sort_by(|a, b| b.title().partial_cmp(a.title()).unwrap());
        }
        SortOrder::Custom => {
            let urls = config.feed_urls();
            feeds.sort_by(|a, b| {
                let a_index = urls.iter().position(|u| a.link() == u).unwrap_or_default();
                let b_index = urls.iter().position(|u| b.link() == u).unwrap_or_default();
                a_index.cmp(&b_index)
            })
        }
        SortOrder::Newest => feeds.sort_by(|a, b| a.last_fetched().cmp(&b.last_fetched())),
        SortOrder::Oldest => feeds.sort_by(|a, b| b.last_fetched().cmp(&a.last_fetched())),
    }
}

impl Repository {
    pub async fn init(config: &Config, app_tx: UnboundedSender<StorageEvent>) -> Result<Self> {
        let db = Database::open_file(config.db_path()).expect("could not open db");

        let (db_tx, db_rx) = mpsc::unbounded_channel::<StorageEvent>();

        // let tick_rate = Duration::from_secs(config.refresh_interval());

        Ok(Self {
            db,
            app_tx,
            db_tx,
            db_rx,
        })
    }

    pub fn get_all_from_db(&mut self, config: &Config) -> anyhow::Result<Vec<Feed>> {
        let feeds = self.db.collection::<Feed>("feeds");
        let cursor = feeds.find(None)?;

        let mut feeds = cursor
            .into_iter()
            .filter_map(|f| f.ok())
            .collect::<Vec<Feed>>();

        sort_feeds(&mut feeds, config);
        Ok(feeds)
    }

    pub fn store_all(&self, feeds: &Vec<Feed>) -> anyhow::Result<()> {
        let collection = self.db.collection::<Feed>("feeds");
        for feed in feeds {
            let query = doc! {  "link": feed.link() };
            let update = bson::to_document(feed)?;

            match collection.find_one(query.clone()) {
                Ok(Some(_)) => {
                    let _ = collection.update_one(query, update);
                }
                Ok(None) => {
                    let _ = collection.insert_one(feed);
                }
                Err(_) => {}
            }
        }

        Ok(())
    }

    pub fn refresh_all(&mut self, config: &Config) {
        let app_tx = self.app_tx.clone();
        let config = config.clone();
        let urls = config.feed_urls().clone();
        let count = urls.len();

        let _ = app_tx.send(StorageEvent::Requesting(count));

        tokio::spawn(async move {
            let futures: Vec<_> = urls.into_iter().map(reqwest::get).collect();
            let handles: Vec<_> = futures
                .into_iter()
                .enumerate()
                .map(|(n, req)| {
                    let app_tx = app_tx.clone();
                    tokio::task::spawn(async move {
                        let res = match req.await {
                            Ok(res) => match res.bytes().await {
                                Ok(bytes) => match Feed::read_from(&bytes[..]) {
                                    Ok(feed) => {
                                        // panic!("{:?}", feed);
                                        Ok(feed)
                                    }
                                    Err(_) => {
                                        // panic!("parse");
                                        Err(FetchErr::Parse)
                                    }
                                },
                                Err(_) => {
                                    // panic!("deserialize");
                                    Err(FetchErr::Deserialize)
                                }
                            },
                            Err(_) => {
                                // panic!("fetch");
                                Err(FetchErr::Request)
                            }
                        };
                        let _ = app_tx.send(StorageEvent::Fetched((n, count)));
                        res
                    })
                })
                .collect();
            let results = futures::future::join_all(handles).await;
            let mut feeds: Vec<_> = results
                .into_iter()
                .filter_map(|handle| match handle {
                    Ok(res) => match res {
                        Ok(channel) => Some(channel),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();

            sort_feeds(&mut feeds, &config);
            app_tx.send(StorageEvent::RetrievedAll(feeds))
        });
    }
}