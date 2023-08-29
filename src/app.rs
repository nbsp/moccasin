use crate::config::Config;
use crate::feed::{Feed, Item};
use crate::repo::{Repository, StorageEvent};
use anyhow::Result;
use clap::Parser;
use std::error;
use std::process::{Child, Command, Stdio};
use std::task::Poll;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tui::widgets::{ListState, ScrollbarState};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Set a custom config file
    #[arg(short, long)]
    pub config: Option<String>,

    /// Set a custom theme, either built-in or a path to a theme file
    #[arg(short = 's', long)]
    pub color_scheme: Option<String>,

    /// Set a custom refresh rate in seconds
    #[arg(short, long)]
    pub interval: Option<u64>,

    /// Set a custom request timeout in seconds
    #[arg(short, long)]
    pub timeout: Option<u64>,
}

/// Application result type.
pub type AppResult<T> = std::result::Result<T, Box<dyn error::Error>>;

#[derive(Debug)]
pub enum LoadState {
    Loading((usize, usize)),
    Errored,
    Done,
}

/// Application.
#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub repo: Repository,
    pub running: bool,
    pub active_view: ActiveView,
    pub feeds: StatefulList<Feed>,
    pub feeds_scroll: ScrollbarState,
    pub items: StatefulList<Item>,
    pub items_scroll: ScrollbarState,
    pub detail_scroll: ScrollbarState,
    pub detail_scroll_index: u16,
    pub load_state: LoadState,
    pub show_keybinds: bool,
    dimensions: (u16, u16),
    rx: UnboundedReceiver<StorageEvent>,
}

impl App {
    pub async fn init(dimensions: (u16, u16), config: Config) -> Result<Self> {
        let urls = config.feed_urls();
        let feeds_count = urls.len() as u16;

        let (tx, rx) = mpsc::unbounded_channel::<StorageEvent>();
        let mut repo = Repository::init(&config, tx).await?;

        let items = repo.get_all_from_db(&config)?;
        repo.refresh_all(&config);

        Ok(Self {
            config,
            repo,
            running: true,
            dimensions,
            active_view: ActiveView::Feeds,
            feeds: StatefulList::<Feed>::with_items(items),
            feeds_scroll: ScrollbarState::default().content_length(feeds_count),
            items: StatefulList::<Item>::default(),
            items_scroll: ScrollbarState::default(),
            detail_scroll: ScrollbarState::default(),
            detail_scroll_index: 0,
            load_state: LoadState::Done,
            show_keybinds: false,
            rx,
        })
    }

    /// Handles the tick event of the terminal.
    pub fn tick(&mut self) {
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        loop {
            match self.rx.poll_recv(&mut cx) {
                Poll::Ready(m) => match m {
                    Some(StorageEvent::Requesting(amount)) => {
                        self.load_state = LoadState::Loading((0, amount));
                    }
                    Some(StorageEvent::Fetched(counts)) => {
                        let counts = match self.load_state {
                            LoadState::Loading((current, total)) => (current + 1, total),
                            _ => counts,
                        };
                        self.load_state = LoadState::Loading(counts);
                    }
                    Some(StorageEvent::RetrievedAll(feeds)) => {
                        let _ = self.repo.store_all(&feeds);
                        self.set_feeds(feeds);
                        self.load_state = LoadState::Done;
                    }
                    None => {
                        break;
                    }
                },
                Poll::Pending => {
                    break;
                }
            }
        }
    }

    /// Set running to false to quit the application.
    pub fn quit(&mut self) {
        self.running = false;
    }

    pub fn set_dimensions(&mut self, dimensions: (u16, u16)) {
        self.dimensions = dimensions;
    }

    pub fn should_render_feeds_scroll(&self) -> bool {
        self.feeds.items().len() as u16 > self.dimensions.1 - 4
    }

    pub fn should_render_items_scroll(&self) -> bool {
        self.items.items().len() as u16 > self.dimensions.1 - 4
    }

    pub fn should_render_detail_scroll(&self) -> bool {
        false
    }

    pub fn current_feed(&self) -> Option<&Feed> {
        self.feeds
            .state
            .selected()
            .and_then(|i| self.feeds.items().get(i))
    }

    pub fn current_item(&self) -> Option<&Item> {
        self.items
            .state
            .selected()
            .and_then(|i| self.items.items().get(i))
    }

    pub fn next_feed(&mut self) {
        self.feeds.next();
        self.feeds_scroll = self.feeds_scroll.position(
            self.feeds
                .state
                .selected()
                .unwrap_or(self.feeds.state.offset()) as u16,
        );

        if let Some(channel) = self.current_feed() {
            self.items.items = channel.items().into();
            self.items_scroll = self
                .items_scroll
                .content_length(self.items.items.len() as u16);
        }
    }

    pub fn prev_feed(&mut self) {
        self.feeds.previous();
        self.feeds_scroll = self.feeds_scroll.position(
            self.feeds
                .state
                .selected()
                .unwrap_or(self.feeds.state.offset()) as u16,
        );

        if let Some(channel) = self.current_feed() {
            self.items.items = channel.items().into();
            self.items_scroll = self
                .items_scroll
                .content_length(self.items.items.len() as u16);
        }
    }

    pub fn next_item(&mut self) {
        self.items.next();
        self.items_scroll = self.items_scroll.position(
            self.items
                .state
                .selected()
                .unwrap_or(self.items.state.offset()) as u16,
        );
    }

    pub fn prev_item(&mut self) {
        self.items.previous();
        self.items_scroll = self.items_scroll.position(
            self.items
                .state
                .selected()
                .unwrap_or(self.items.state.offset()) as u16,
        );
    }

    pub fn next_view(&mut self, wrap: bool) {
        let has_current_feed = self.current_feed().is_some();
        let has_current_item = self.current_item().is_some();

        if !has_current_feed {
            self.active_view = ActiveView::Feeds;
            return;
        }

        if let Some(next_view) = match self.active_view {
            ActiveView::Feeds => {
                if self.items.state.selected().is_none() {
                    self.next_item();
                }
                Some(ActiveView::Items)
            }
            ActiveView::Items => {
                if has_current_item {
                    Some(ActiveView::Detail)
                } else if wrap {
                    Some(ActiveView::Feeds)
                } else {
                    None
                }
            }
            ActiveView::Detail => {
                if wrap {
                    Some(ActiveView::Feeds)
                } else {
                    None
                }
            }
        } {
            self.active_view = next_view;
        }
    }

    pub fn prev_view(&mut self, wrap: bool) {
        let has_current_feed = self.current_feed().is_some();
        let has_current_item = self.current_item().is_some();

        if !has_current_feed {
            self.active_view = ActiveView::Feeds;
            return;
        }

        if let Some(next_view) = match self.active_view {
            ActiveView::Feeds => {
                if wrap && has_current_item {
                    Some(ActiveView::Detail)
                } else if wrap {
                    Some(ActiveView::Items)
                } else {
                    None
                }
            }
            ActiveView::Items => Some(ActiveView::Feeds),
            ActiveView::Detail => Some(ActiveView::Items),
        } {
            self.active_view = next_view;
        }
    }

    pub fn next(&mut self) {
        match self.active_view {
            ActiveView::Feeds => {
                self.reset_items_scroll();
                self.reset_detail_scroll();
                self.next_feed();
            }
            ActiveView::Items => {
                self.reset_detail_scroll();
                self.next_item();
            }
            ActiveView::Detail => {
                self.detail_scroll_index = self.detail_scroll_index.saturating_add(1);
                self.detail_scroll.next();
            }
        }
    }

    pub fn prev(&mut self) {
        match self.active_view {
            ActiveView::Feeds => {
                self.reset_items_scroll();
                self.reset_detail_scroll();
                self.prev_feed();
            }
            ActiveView::Items => {
                self.reset_detail_scroll();
                self.prev_item();
            }
            ActiveView::Detail => {
                self.detail_scroll_index = self.detail_scroll_index.saturating_sub(1);
                self.detail_scroll.prev();
            }
        }
    }

    pub fn unselect(&mut self) {
        if self.current_item().is_some() {
            self.items.state.select(None);
        } else {
            self.feeds.state.select(None);
        }
        self.prev_view(false);
    }

    pub fn open(&mut self) {
        match self.active_view {
            ActiveView::Feeds => {
                if let Some(feed) = self.current_feed() {
                    let link = feed.link();
                    let _ = App::open_link(link);
                }
            }
            ActiveView::Items => {
                if let Some(item) = self.current_item() {
                    if let Some(link) = item.link() {
                        let _ = App::open_link(link);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn open_config(&self) -> Option<Child> {
        if let Some(cfg_path) = self.config.config_file_path().as_path().to_str() {
            Self::open_link(cfg_path)
        } else {
            None
        }
    }

    pub fn refresh_all(&mut self) {
        let _ = self.repo.refresh_all(&self.config);
    }

    pub fn toggle_keybinds(&mut self) {
        self.show_keybinds = !self.show_keybinds;
    }

    fn set_feeds(&mut self, feeds: Vec<Feed>) {
        self.feeds.items = feeds;
        // self.items.state.select(None);
        // self.active_view = ActiveView::Feeds;
    }

    fn reset_items_scroll(&mut self) {
        self.items.state.select(None);
        self.items_scroll = self.items_scroll.position(0);
    }

    fn reset_detail_scroll(&mut self) {
        self.detail_scroll_index = 0;
        self.detail_scroll = self.detail_scroll.position(0);
    }

    fn open_link(link: &str) -> Option<Child> {
        let null = Stdio::null();
        if cfg!(target_os = "windows") {
            Command::new("rundll32")
                .args(["url.dll,FileProtocolHandler", link])
                .stdout(null)
                .spawn()
                .ok()
        } else if cfg!(target_os = "macos") {
            Command::new("open").arg(link).stdout(null).spawn().ok()
        } else if cfg!(target_os = "linux") {
            Command::new("xdg-open").arg(link).stdout(null).spawn().ok()
        } else {
            None
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ActiveView {
    Feeds,
    Items,
    Detail,
}

#[derive(Default, Debug)]
pub struct StatefulList<T> {
    pub state: ListState,
    pub items: Vec<T>,
}

impl<T> StatefulList<T> {
    fn with_items(items: Vec<T>) -> StatefulList<T> {
        StatefulList {
            state: ListState::default(),
            items,
        }
    }

    fn next(&mut self) {
        if self.items.len() == 0 {
            return;
        }

        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.items.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.items.len() == 0 {
            return;
        }

        let i = match self.state.selected() {
            Some(i) => {
                if i <= 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    #[allow(dead_code)]
    fn unselect(&mut self) {
        self.state.select(None);
    }

    pub fn items(&self) -> &Vec<T> {
        &self.items
    }
}
