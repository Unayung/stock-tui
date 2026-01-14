use anyhow::Result;
use chrono::Local;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table, TableState, Tabs},
    Frame, Terminal,
};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const CACHE_DURATION_SECS: u64 = 60;
const HISTORICAL_CACHE_DURATION_SECS: u64 = 6 * 60 * 60; // 6 hours for historical data

/// Message sent from background fetch thread to main thread
#[derive(Debug)]
struct FetchResult {
    symbol: String,
    price_data: Option<PriceData>,
}

/// Message indicating a batch fetch has completed
#[derive(Debug)]
enum FetchMessage {
    /// Individual price result
    Price(FetchResult),
    /// Exchange rate result
    ExchangeRate(f64),
    /// All fetches in this batch are complete
    BatchComplete,
}

/// Tracks clickable UI regions for mouse interaction
#[derive(Default, Clone)]
struct ClickableRegions {
    /// Portfolio tab areas: (rect, portfolio_index) - index 0 = "ALL" combined view
    portfolio_tabs: Vec<(Rect, usize)>,
    /// Taiwan stocks table area
    tw_table: Rect,
    /// US stocks table area
    us_table: Rect,
    /// Individual TW stock rows: (rect, row_index)
    tw_rows: Vec<(Rect, usize)>,
    /// Individual US stock rows: (rect, row_index)
    us_rows: Vec<(Rect, usize)>,
    /// Footer button regions: (rect, action_name)
    footer_buttons: Vec<(Rect, &'static str)>,
}

#[derive(Clone, Debug)]
struct Stock {
    symbol: String,
    display: String,
    name: String,
    quantity: f64,
    cost_basis: f64,
    price_data: Option<PriceData>,
    historical: Option<HistoricalData>,
    portfolio_name: String,
}

#[derive(Clone, Debug)]
struct PriceData {
    price: f64,
    #[allow(dead_code)]
    change: f64, // Kept for potential future use (e.g., displaying absolute change)
    change_percent: f64,
}

#[derive(Clone, Debug)]
struct HistoricalData {
    #[allow(dead_code)]
    timestamps: Vec<i64>, // Kept for potential future use (e.g., date labels)
    closes: Vec<f64>,
    last_fetched: Instant,
}

#[derive(Clone, Debug)]
struct Portfolio {
    name: String,
    file_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SortColumn {
    Price,
    Change,
    Quantity,
    Gain,
    GainPercent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug)]
enum InputMode {
    Normal,
    AddStock(AddStockState),
    EditStock(EditStockState),
    DeleteConfirm(String),
    NewPortfolio(String),
    DetailView(String), // Symbol being viewed in detail
}

#[derive(Debug, Default)]
struct AddStockState {
    step: usize,
    symbol: String,
    display: String,
    name: String,
    quantity: String,
    cost_basis: String,
}

#[derive(Debug, Default)]
struct EditStockState {
    symbol: String,
    quantity: String,
    cost_basis: String,
}

struct App {
    portfolios: Vec<Portfolio>,
    current_portfolio_idx: usize,
    view_combined: bool,
    stocks: Vec<Stock>,
    combined_stocks: Vec<Stock>,
    tw_stocks: Vec<Stock>,
    us_stocks: Vec<Stock>,
    combined_tw_stocks: Vec<Stock>,
    combined_us_stocks: Vec<Stock>,
    usd_twd_rate: f64,
    active_section: usize, // 0 = TW, 1 = US
    table_state_tw: TableState,
    table_state_us: TableState,
    last_update: Instant,
    input_mode: InputMode,
    cache: HashMap<String, (PriceData, Instant)>,
    historical_cache: HashMap<String, HistoricalData>,
    sort_column: Option<SortColumn>,
    sort_direction: SortDirection,
    hide_positions: bool,   // Toggle with 'H' to hide cost/quantity/gain for privacy
    live_mode: bool,        // Toggle with 'L' for auto-refresh every 5 seconds
    show_gain_amount: bool, // Toggle with 'T' to switch between gain amount and percentage in titles
    last_live_refresh: Instant,
    clickable_regions: ClickableRegions,
    // Async fetch infrastructure
    fetch_receiver: Receiver<FetchMessage>,
    fetch_sender: Sender<FetchMessage>,
    is_fetching: bool, // True when background fetch is in progress
}

impl App {
    fn new() -> Result<Self> {
        let (fetch_sender, fetch_receiver) = mpsc::channel();
        let mut app = App {
            portfolios: Vec::new(),
            current_portfolio_idx: 0,
            view_combined: false,
            stocks: Vec::new(),
            combined_stocks: Vec::new(),
            tw_stocks: Vec::new(),
            us_stocks: Vec::new(),
            combined_tw_stocks: Vec::new(),
            combined_us_stocks: Vec::new(),
            usd_twd_rate: 32.0,
            active_section: 0,
            table_state_tw: TableState::default(),
            table_state_us: TableState::default(),
            last_update: Instant::now(),
            input_mode: InputMode::Normal,
            cache: HashMap::new(),
            historical_cache: HashMap::new(),
            sort_column: Some(SortColumn::Change), // Default sort by change %
            sort_direction: SortDirection::Descending,
            hide_positions: false,
            live_mode: false,
            show_gain_amount: false, // Start with percentage display
            last_live_refresh: Instant::now(),
            clickable_regions: ClickableRegions::default(),
            fetch_receiver,
            fetch_sender,
            is_fetching: false,
        };
        app.load_portfolios()?;
        app.refresh_data()?;
        Ok(app)
    }

    fn is_demo_mode() -> bool {
        std::env::var("DEMO").map(|v| v == "true" || v == "1").unwrap_or(false)
    }

    fn portfolios_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config/stock-tui/portfolios")
    }

    fn cache_dir() -> PathBuf {
        PathBuf::from("/tmp/stock-tui")
    }

    fn load_portfolios(&mut self) -> Result<()> {
        // Demo mode: load from demo.conf in current directory or next to executable
        if Self::is_demo_mode() {
            let demo_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.join("demo.conf")))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("demo.conf"));

            if demo_path.exists() {
                self.portfolios = vec![Portfolio {
                    name: "demo".to_string(),
                    file_path: demo_path,
                }];
                return Ok(());
            }
        }

        let dir = Self::portfolios_dir();
        fs::create_dir_all(&dir)?;

        self.portfolios = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|ext| ext == "conf").unwrap_or(false))
            .map(|e| {
                let path = e.path();
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                Portfolio {
                    name,
                    file_path: path,
                }
            })
            .collect();

        // Sort with 'main' first
        self.portfolios.sort_by(|a, b| {
            if a.name == "main" {
                std::cmp::Ordering::Less
            } else if b.name == "main" {
                std::cmp::Ordering::Greater
            } else {
                a.name.cmp(&b.name)
            }
        });

        if self.portfolios.is_empty() {
            let main_path = dir.join("main.conf");
            fs::write(&main_path, "# Stock Portfolio Configuration\n# Format: SYMBOL|Display Name|Description|Quantity|Cost Basis\n")?;
            self.portfolios.push(Portfolio {
                name: "main".to_string(),
                file_path: main_path,
            });
        }

        Ok(())
    }

    fn load_stocks_from_file(path: &PathBuf) -> Result<Vec<Stock>> {
        let mut stocks = Vec::new();
        if !path.exists() {
            return Ok(stocks);
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() >= 3 {
                stocks.push(Stock {
                    symbol: parts[0].trim().to_string(),
                    display: parts[1].trim().to_string(),
                    name: parts[2].trim().to_string(),
                    quantity: parts.get(3).and_then(|s| s.trim().parse().ok()).unwrap_or(0.0),
                    cost_basis: parts.get(4).and_then(|s| s.trim().parse().ok()).unwrap_or(0.0),
                    price_data: None,
                    historical: None,
                    portfolio_name: String::new(),
                });
            }
        }

        Ok(stocks)
    }

    fn save_stocks(&self, portfolio_name: &str, stocks: &[Stock]) -> Result<()> {
        let path = Self::portfolios_dir().join(format!("{}.conf", portfolio_name));
        let mut file = File::create(&path)?;

        writeln!(file, "# Stock Portfolio Configuration")?;
        writeln!(file, "# Format: SYMBOL|Display Name|Description|Quantity|Cost Basis")?;
        writeln!(file)?;

        let tw_stocks: Vec<_> = stocks.iter().filter(|s| s.symbol.contains(".TW")).collect();
        let us_stocks: Vec<_> = stocks.iter().filter(|s| !s.symbol.contains(".TW")).collect();

        if !tw_stocks.is_empty() {
            writeln!(file, "# Taiwan Stocks")?;
            for s in tw_stocks {
                writeln!(file, "{}|{}|{}|{}|{}", s.symbol, s.display, s.name, s.quantity, s.cost_basis)?;
            }
            writeln!(file)?;
        }

        if !us_stocks.is_empty() {
            writeln!(file, "# US Stocks")?;
            for s in us_stocks {
                writeln!(file, "{}|{}|{}|{}|{}", s.symbol, s.display, s.name, s.quantity, s.cost_basis)?;
            }
        }

        Ok(())
    }

    fn fetch_price(&mut self, symbol: &str) -> Option<PriceData> {
        // Check cache first
        if let Some((data, time)) = self.cache.get(symbol) {
            if time.elapsed().as_secs() < CACHE_DURATION_SECS {
                return Some(data.clone());
            }
        }

        // Try file cache
        fs::create_dir_all(Self::cache_dir()).ok();
        let cache_file = Self::cache_dir().join(format!("{}.cache", symbol.replace('.', "_")));

        if let Ok(metadata) = fs::metadata(&cache_file) {
            if let Ok(modified) = metadata.modified() {
                if modified.elapsed().map(|d| d.as_secs() < CACHE_DURATION_SECS).unwrap_or(false) {
                    if let Ok(content) = fs::read_to_string(&cache_file) {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
                            let price_data = PriceData {
                                price: data["price"].as_f64().unwrap_or(0.0),
                                change: data["change"].as_f64().unwrap_or(0.0),
                                change_percent: data["change_percent"].as_f64().unwrap_or(0.0),
                            };
                            self.cache.insert(symbol.to_string(), (price_data.clone(), Instant::now()));
                            return Some(price_data);
                        }
                    }
                }
            }
        }

        // Use chart API (v7 quote API is restricted by Yahoo)
        let urls = [
            format!("https://query2.finance.yahoo.com/v8/finance/chart/{}", symbol),
            format!("https://query1.finance.yahoo.com/v8/finance/chart/{}", symbol),
        ];

        for url in &urls {
            if let Ok(response) = reqwest::blocking::Client::new()
                .get(url)
                .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
                .timeout(Duration::from_secs(5))
                .send()
            {
                if let Ok(data) = response.json::<serde_json::Value>() {
                    if let Some(result) = data["chart"]["result"].get(0) {
                        let meta = &result["meta"];
                        let price = meta["regularMarketPrice"].as_f64()
                            .or_else(|| meta["previousClose"].as_f64());
                        let prev_close = meta["previousClose"].as_f64()
                            .or_else(|| meta["chartPreviousClose"].as_f64());

                        if let (Some(price), Some(prev)) = (price, prev_close) {
                            let change = price - prev;
                            let change_percent = (change / prev) * 100.0;

                            let price_data = PriceData { price, change, change_percent };

                            // Save to file cache
                            let cache_json = serde_json::json!({
                                "price": price,
                                "change": change,
                                "change_percent": change_percent
                            });
                            let _ = fs::write(&cache_file, cache_json.to_string());

                            self.cache.insert(symbol.to_string(), (price_data.clone(), Instant::now()));
                            return Some(price_data);
                        }
                    }
                }
            }
        }

        None
    }

    fn fetch_exchange_rate(&mut self) -> f64 {
        if let Some(data) = self.fetch_price("USDTWD=X") {
            data.price
        } else {
            32.0
        }
    }

    /// Start an async background refresh of all stock prices
    /// Results will be sent through the fetch_receiver channel
    fn start_async_refresh(&mut self) {
        if self.is_fetching {
            return; // Already fetching
        }

        self.is_fetching = true;
        let sender = self.fetch_sender.clone();

        // Collect all symbols we need to fetch
        let symbols: Vec<String> = if self.view_combined {
            self.combined_stocks.iter().map(|s| s.symbol.clone()).collect()
        } else {
            self.stocks.iter().map(|s| s.symbol.clone()).collect()
        };

        // Spawn background thread
        thread::spawn(move || {
            // Fetch exchange rate first
            if let Some(rate) = fetch_price_blocking("USDTWD=X") {
                let _ = sender.send(FetchMessage::ExchangeRate(rate.price));
            }

            // Fetch each stock price
            for symbol in symbols {
                let price_data = fetch_price_blocking(&symbol);
                let _ = sender.send(FetchMessage::Price(FetchResult {
                    symbol,
                    price_data,
                }));
            }

            // Signal completion
            let _ = sender.send(FetchMessage::BatchComplete);
        });
    }

    /// Process any pending fetch results from background thread
    /// Returns true if any updates were received
    fn process_fetch_results(&mut self) -> bool {
        let mut updated = false;

        // Non-blocking receive of all pending messages
        while let Ok(msg) = self.fetch_receiver.try_recv() {
            match msg {
                FetchMessage::Price(result) => {
                    // Update price in all stock vectors
                    if let Some(ref price_data) = result.price_data {
                        // Update cache
                        self.cache.insert(result.symbol.clone(), (price_data.clone(), Instant::now()));

                        // Update all stock vectors
                        for stock in self.stocks.iter_mut()
                            .chain(self.tw_stocks.iter_mut())
                            .chain(self.us_stocks.iter_mut())
                            .chain(self.combined_stocks.iter_mut())
                            .chain(self.combined_tw_stocks.iter_mut())
                            .chain(self.combined_us_stocks.iter_mut())
                        {
                            if stock.symbol == result.symbol {
                                stock.price_data = Some(price_data.clone());
                            }
                        }
                    }
                    updated = true;
                }
                FetchMessage::ExchangeRate(rate) => {
                    self.usd_twd_rate = rate;
                    updated = true;
                }
                FetchMessage::BatchComplete => {
                    self.is_fetching = false;
                    self.last_update = Instant::now();
                    self.sort_stocks(); // Re-sort after all prices updated
                    updated = true;
                }
            }
        }

        updated
    }

    fn fetch_historical(&mut self, symbol: &str) -> Option<HistoricalData> {
        // Check in-memory cache first
        if let Some(data) = self.historical_cache.get(symbol) {
            if data.last_fetched.elapsed().as_secs() < HISTORICAL_CACHE_DURATION_SECS {
                return Some(data.clone());
            }
        }

        // Try file cache
        fs::create_dir_all(Self::cache_dir()).ok();
        let cache_file = Self::cache_dir().join(format!("{}_history.json", symbol.replace('.', "_")));

        if let Ok(metadata) = fs::metadata(&cache_file) {
            if let Ok(modified) = metadata.modified() {
                if modified.elapsed().map(|d| d.as_secs() < HISTORICAL_CACHE_DURATION_SECS).unwrap_or(false) {
                    if let Ok(content) = fs::read_to_string(&cache_file) {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
                            let timestamps: Vec<i64> = data["timestamps"]
                                .as_array()
                                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                                .unwrap_or_default();
                            let closes: Vec<f64> = data["closes"]
                                .as_array()
                                .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
                                .unwrap_or_default();

                            if !timestamps.is_empty() && !closes.is_empty() {
                                let historical = HistoricalData {
                                    timestamps,
                                    closes,
                                    last_fetched: Instant::now(),
                                };
                                self.historical_cache.insert(symbol.to_string(), historical.clone());
                                return Some(historical);
                            }
                        }
                    }
                }
            }
        }

        // Fetch from Yahoo Finance API
        let url = format!(
            "https://query2.finance.yahoo.com/v8/finance/chart/{}?interval=1d&range=1mo",
            symbol
        );

        if let Ok(response) = reqwest::blocking::Client::new()
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
            .timeout(Duration::from_secs(10))
            .send()
        {
            if let Ok(data) = response.json::<serde_json::Value>() {
                if let Some(result) = data["chart"]["result"].get(0) {
                    let timestamps: Vec<i64> = result["timestamp"]
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                        .unwrap_or_default();

                    let closes: Vec<f64> = result["indicators"]["quote"][0]["close"]
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
                        .unwrap_or_default();

                    if !timestamps.is_empty() && !closes.is_empty() {
                        // Save to file cache
                        let cache_json = serde_json::json!({
                            "timestamps": timestamps,
                            "closes": closes
                        });
                        let _ = fs::write(&cache_file, cache_json.to_string());

                        let historical = HistoricalData {
                            timestamps,
                            closes,
                            last_fetched: Instant::now(),
                        };
                        self.historical_cache.insert(symbol.to_string(), historical.clone());
                        return Some(historical);
                    }
                }
            }
        }

        None
    }

    /// Calculate trend from historical data: compare first 5 days avg vs last 5 days avg
    fn calculate_trend(closes: &[f64]) -> (&'static str, Color) {
        if closes.len() < 10 {
            return ("→", Color::Gray);
        }

        let first_avg: f64 = closes.iter().take(5).sum::<f64>() / 5.0;
        let last_avg: f64 = closes.iter().rev().take(5).sum::<f64>() / 5.0;
        let change_pct = ((last_avg - first_avg) / first_avg) * 100.0;

        if change_pct > 1.0 {
            ("⬆", Color::Green)
        } else if change_pct < -1.0 {
            ("⬇", Color::Red)
        } else {
            ("→", Color::Gray)
        }
    }

    fn refresh_data(&mut self) -> Result<()> {
        self.usd_twd_rate = self.fetch_exchange_rate();

        // Load current portfolio stocks with prices
        let (file_path, portfolio_name) = if let Some(portfolio) = self.portfolios.get(self.current_portfolio_idx) {
            (portfolio.file_path.clone(), portfolio.name.clone())
        } else {
            return Ok(());
        };

        let mut stocks = Self::load_stocks_from_file(&file_path)?;
        for stock in &mut stocks {
            stock.price_data = self.fetch_price(&stock.symbol);
            stock.portfolio_name = portfolio_name.clone();
        }
        self.stocks = stocks;

        // Split into TW and US
        self.tw_stocks = self.stocks.iter().filter(|s| s.symbol.contains(".TW")).cloned().collect();
        self.us_stocks = self.stocks.iter().filter(|s| !s.symbol.contains(".TW")).cloned().collect();

        // Load combined stocks (aggregated)
        self.load_combined_stocks()?;

        self.last_update = Instant::now();
        Ok(())
    }

    fn load_combined_stocks(&mut self) -> Result<()> {
        let mut aggregated: HashMap<String, Stock> = HashMap::new();
        let mut portfolio_map: HashMap<String, Vec<String>> = HashMap::new();

        for portfolio in &self.portfolios {
            let stocks = Self::load_stocks_from_file(&portfolio.file_path)?;
            for stock in stocks {
                portfolio_map
                    .entry(stock.symbol.clone())
                    .or_default()
                    .push(portfolio.name.clone());

                if let Some(existing) = aggregated.get_mut(&stock.symbol) {
                    let old_qty = existing.quantity;
                    let old_cost = existing.cost_basis;
                    let new_qty = stock.quantity;
                    let new_cost = stock.cost_basis;

                    let combined_qty = old_qty + new_qty;
                    let weighted_cost = if combined_qty > 0.0 {
                        ((old_qty * old_cost) + (new_qty * new_cost)) / combined_qty
                    } else {
                        0.0
                    };

                    existing.quantity = combined_qty;
                    existing.cost_basis = weighted_cost;
                } else {
                    aggregated.insert(stock.symbol.clone(), stock);
                }
            }
        }

        // Fetch prices for combined stocks
        self.combined_stocks = aggregated
            .into_iter()
            .map(|(symbol, mut stock)| {
                stock.price_data = self.fetch_price(&symbol);
                let portfolios = portfolio_map.get(&symbol).unwrap();
                stock.portfolio_name = if portfolios.len() > 1 {
                    portfolios.join("+")
                } else {
                    portfolios.first().cloned().unwrap_or_default()
                };
                stock
            })
            .collect();
        self.combined_tw_stocks = self.combined_stocks.iter().filter(|s| s.symbol.contains(".TW")).cloned().collect();
        self.combined_us_stocks = self.combined_stocks.iter().filter(|s| !s.symbol.contains(".TW")).cloned().collect();

        self.sort_stocks();

        Ok(())
    }

    fn sort_stocks(&mut self) {
        let sort_col = self.sort_column;
        let sort_dir = self.sort_direction;
        let usd_twd = self.usd_twd_rate;

        let sorter = |a: &Stock, b: &Stock| -> std::cmp::Ordering {
            let cmp = match sort_col {
                Some(SortColumn::Price) => {
                    let a_val = a.price_data.as_ref().map(|d| d.price).unwrap_or(0.0);
                    let b_val = b.price_data.as_ref().map(|d| d.price).unwrap_or(0.0);
                    a_val.partial_cmp(&b_val).unwrap_or(std::cmp::Ordering::Equal)
                }
                Some(SortColumn::Change) => {
                    let a_val = a.price_data.as_ref().map(|d| d.change_percent).unwrap_or(f64::NEG_INFINITY);
                    let b_val = b.price_data.as_ref().map(|d| d.change_percent).unwrap_or(f64::NEG_INFINITY);
                    a_val.partial_cmp(&b_val).unwrap_or(std::cmp::Ordering::Equal)
                }
                Some(SortColumn::Quantity) => {
                    a.quantity.partial_cmp(&b.quantity).unwrap_or(std::cmp::Ordering::Equal)
                }
                Some(SortColumn::Gain) => {
                    let a_gain = if a.quantity > 0.0 && a.cost_basis > 0.0 {
                        if let Some(ref d) = a.price_data {
                            let mut g = a.quantity * d.price - a.quantity * a.cost_basis;
                            if !a.symbol.contains(".TW") { g *= usd_twd; }
                            g
                        } else { 0.0 }
                    } else { 0.0 };
                    let b_gain = if b.quantity > 0.0 && b.cost_basis > 0.0 {
                        if let Some(ref d) = b.price_data {
                            let mut g = b.quantity * d.price - b.quantity * b.cost_basis;
                            if !b.symbol.contains(".TW") { g *= usd_twd; }
                            g
                        } else { 0.0 }
                    } else { 0.0 };
                    a_gain.partial_cmp(&b_gain).unwrap_or(std::cmp::Ordering::Equal)
                }
                Some(SortColumn::GainPercent) => {
                    let a_pct = if a.quantity > 0.0 && a.cost_basis > 0.0 {
                        if let Some(ref d) = a.price_data {
                            ((d.price - a.cost_basis) / a.cost_basis) * 100.0
                        } else { 0.0 }
                    } else { 0.0 };
                    let b_pct = if b.quantity > 0.0 && b.cost_basis > 0.0 {
                        if let Some(ref d) = b.price_data {
                            ((d.price - b.cost_basis) / b.cost_basis) * 100.0
                        } else { 0.0 }
                    } else { 0.0 };
                    a_pct.partial_cmp(&b_pct).unwrap_or(std::cmp::Ordering::Equal)
                }
                None => std::cmp::Ordering::Equal,
            };

            match sort_dir {
                SortDirection::Ascending => cmp,
                SortDirection::Descending => cmp.reverse(),
            }
        };

        self.tw_stocks.sort_by(sorter);
        self.us_stocks.sort_by(sorter);
        self.combined_tw_stocks.sort_by(sorter);
        self.combined_us_stocks.sort_by(sorter);
    }

    fn toggle_sort(&mut self, column: SortColumn) {
        if self.sort_column == Some(column) {
            // Toggle direction
            self.sort_direction = match self.sort_direction {
                SortDirection::Ascending => SortDirection::Descending,
                SortDirection::Descending => SortDirection::Ascending,
            };
        } else {
            // New column, default to descending
            self.sort_column = Some(column);
            self.sort_direction = SortDirection::Descending;
        }
        self.sort_stocks();
    }

    fn get_active_tw_stocks(&self) -> &[Stock] {
        if self.view_combined {
            &self.combined_tw_stocks
        } else {
            &self.tw_stocks
        }
    }

    fn get_active_us_stocks(&self) -> &[Stock] {
        if self.view_combined {
            &self.combined_us_stocks
        } else {
            &self.us_stocks
        }
    }

    fn calculate_summary(&self) -> (f64, f64, f64, f64, usize, usize) {
        let stocks = if self.view_combined {
            &self.combined_stocks
        } else {
            &self.stocks
        };

        let mut total_cost = 0.0;
        let mut total_value = 0.0;
        let mut holdings = 0;

        for stock in stocks {
            if stock.quantity > 0.0 {
                if let Some(ref data) = stock.price_data {
                    let mut cost = stock.quantity * stock.cost_basis;
                    let mut value = stock.quantity * data.price;

                    if !stock.symbol.contains(".TW") {
                        cost *= self.usd_twd_rate;
                        value *= self.usd_twd_rate;
                    }

                    total_cost += cost;
                    total_value += value;
                    holdings += 1;
                }
            }
        }

        let total_gain = total_value - total_cost;
        let total_gain_percent = if total_cost > 0.0 {
            (total_gain / total_cost) * 100.0
        } else {
            0.0
        };

        (total_cost, total_value, total_gain, total_gain_percent, stocks.len(), holdings)
    }

    // Returns: (tw_value, tw_gain, tw_gain_pct, us_value_usd, us_gain_usd, us_gain_pct)
    fn calculate_market_summary(&self) -> (f64, f64, f64, f64, f64, f64) {
        let stocks = if self.view_combined {
            &self.combined_stocks
        } else {
            &self.stocks
        };

        let mut tw_cost = 0.0;
        let mut tw_value = 0.0;
        let mut us_cost = 0.0;
        let mut us_value = 0.0;

        for stock in stocks {
            if stock.quantity > 0.0 {
                if let Some(ref data) = stock.price_data {
                    let cost = stock.quantity * stock.cost_basis;
                    let value = stock.quantity * data.price;

                    if stock.symbol.contains(".TW") {
                        tw_cost += cost;
                        tw_value += value;
                    } else {
                        us_cost += cost;
                        us_value += value;
                    }
                }
            }
        }

        let tw_gain = tw_value - tw_cost;
        let tw_gain_pct = if tw_cost > 0.0 { (tw_gain / tw_cost) * 100.0 } else { 0.0 };

        let us_gain = us_value - us_cost;
        let us_gain_pct = if us_cost > 0.0 { (us_gain / us_cost) * 100.0 } else { 0.0 };

        (tw_value, tw_gain, tw_gain_pct, us_value, us_gain, us_gain_pct)
    }

    fn next_row(&mut self) {
        let len = if self.active_section == 0 {
            if self.view_combined { self.combined_tw_stocks.len() } else { self.tw_stocks.len() }
        } else {
            if self.view_combined { self.combined_us_stocks.len() } else { self.us_stocks.len() }
        };

        if len == 0 {
            return;
        }

        let state = if self.active_section == 0 {
            &mut self.table_state_tw
        } else {
            &mut self.table_state_us
        };

        let i = match state.selected() {
            Some(i) => (i + 1).min(len - 1),
            None => 0,
        };
        state.select(Some(i));
    }

    fn prev_row(&mut self) {
        let state = if self.active_section == 0 {
            &mut self.table_state_tw
        } else {
            &mut self.table_state_us
        };

        let i = match state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        state.select(Some(i));
    }

    fn get_selected_stock(&self) -> Option<&Stock> {
        let (stocks, state) = if self.active_section == 0 {
            (self.get_active_tw_stocks(), &self.table_state_tw)
        } else {
            (self.get_active_us_stocks(), &self.table_state_us)
        };

        state.selected().and_then(|i| stocks.get(i))
    }

    fn add_stock(&mut self, symbol: String, display: String, name: String, quantity: f64, cost_basis: f64) -> Result<()> {
        if let Some(portfolio) = self.portfolios.get(self.current_portfolio_idx) {
            let mut stocks = Self::load_stocks_from_file(&portfolio.file_path)?;
            stocks.push(Stock {
                symbol,
                display,
                name,
                quantity,
                cost_basis,
                price_data: None,
                historical: None,
                portfolio_name: portfolio.name.clone(),
            });
            self.save_stocks(&portfolio.name, &stocks)?;
        }
        Ok(())
    }

    fn edit_stock(&mut self, symbol: &str, quantity: f64, cost_basis: f64) -> Result<()> {
        if let Some(portfolio) = self.portfolios.get(self.current_portfolio_idx) {
            let mut stocks = Self::load_stocks_from_file(&portfolio.file_path)?;
            if let Some(stock) = stocks.iter_mut().find(|s| s.symbol == symbol) {
                stock.quantity = quantity;
                stock.cost_basis = cost_basis;
            }
            self.save_stocks(&portfolio.name, &stocks)?;
        }
        Ok(())
    }

    fn delete_stock(&mut self, symbol: &str) -> Result<()> {
        if let Some(portfolio) = self.portfolios.get(self.current_portfolio_idx) {
            let mut stocks = Self::load_stocks_from_file(&portfolio.file_path)?;
            stocks.retain(|s| s.symbol != symbol);
            self.save_stocks(&portfolio.name, &stocks)?;
        }
        Ok(())
    }

    fn create_portfolio(&mut self, name: &str) -> Result<()> {
        let path = Self::portfolios_dir().join(format!("{}.conf", name));
        fs::write(&path, "# Stock Portfolio Configuration\n# Format: SYMBOL|Display Name|Description|Quantity|Cost Basis\n")?;
        self.load_portfolios()?;
        Ok(())
    }
}

/// Standalone blocking price fetch for use in background threads
/// Does not use any caching - always fetches fresh data
fn fetch_price_blocking(symbol: &str) -> Option<PriceData> {
    // Use chart API (v7 quote API is restricted by Yahoo)
    let urls = [
        format!("https://query2.finance.yahoo.com/v8/finance/chart/{}", symbol),
        format!("https://query1.finance.yahoo.com/v8/finance/chart/{}", symbol),
    ];

    for url in &urls {
        if let Ok(response) = reqwest::blocking::Client::new()
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
            .timeout(Duration::from_secs(5))
            .send()
        {
            if let Ok(data) = response.json::<serde_json::Value>() {
                if let Some(result) = data["chart"]["result"].get(0) {
                    let meta = &result["meta"];
                    let price = meta["regularMarketPrice"].as_f64()
                        .or_else(|| meta["previousClose"].as_f64());
                    let prev_close = meta["previousClose"].as_f64()
                        .or_else(|| meta["chartPreviousClose"].as_f64());

                    if let (Some(price), Some(prev)) = (price, prev_close) {
                        let change = price - prev;
                        let change_percent = (change / prev) * 100.0;
                        return Some(PriceData { price, change, change_percent });
                    }
                }
            }
        }
    }

    None
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new()?;
    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Error: {err:?}");
    }

    Ok(())
}

enum Action {
    None,
    Quit,
    AddStock(String, String, String, f64, f64),
    EditStock(String, f64, f64),
    DeleteStock(String),
    CreatePortfolio(String),
    Refresh,
    SwitchPortfolio(usize),
    Sort(SortColumn),
    ToggleLive,
    ToggleHide,
    SelectTwRow(usize),
    SelectUsRow(usize),
    ViewCombined,
    OpenDetail,
}

const LIVE_REFRESH_INTERVAL_SECS: u64 = 5;

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        // Process any pending fetch results from background thread (non-blocking)
        app.process_fetch_results();

        terminal.draw(|f| ui(f, app))?;
        // Note: clickable_regions are updated during ui() rendering

        // Live mode: start async refresh every 5 seconds (non-blocking)
        if app.live_mode
            && !app.is_fetching
            && matches!(app.input_mode, InputMode::Normal)
            && app.last_live_refresh.elapsed().as_secs() >= LIVE_REFRESH_INTERVAL_SECS
        {
            app.last_live_refresh = Instant::now();
            app.start_async_refresh();
        }

        if event::poll(Duration::from_millis(100))? {
            let event = event::read()?;

            let action = match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_input(app, key.code)
                }
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse.kind, mouse.column, mouse.row)
                }
                _ => Action::None,
            };

            match action {
                    Action::Quit => return Ok(()),
                    Action::AddStock(symbol, display, name, qty, cost) => {
                        app.add_stock(symbol, display, name, qty, cost)?;
                        app.refresh_data()?;
                        app.input_mode = InputMode::Normal;
                    }
                    Action::EditStock(symbol, qty, cost) => {
                        app.edit_stock(&symbol, qty, cost)?;
                        app.refresh_data()?;
                        app.input_mode = InputMode::Normal;
                    }
                    Action::DeleteStock(symbol) => {
                        app.delete_stock(&symbol)?;
                        app.refresh_data()?;
                        app.input_mode = InputMode::Normal;
                    }
                    Action::CreatePortfolio(name) => {
                        app.create_portfolio(&name)?;
                        app.input_mode = InputMode::Normal;
                    }
                    Action::Refresh => {
                        if !app.is_fetching {
                            app.cache.clear();
                            app.historical_cache.clear();
                            app.start_async_refresh();
                        }
                    }
                    Action::SwitchPortfolio(idx) => {
                        app.view_combined = false;
                        app.current_portfolio_idx = idx;
                        app.refresh_data()?;
                        app.table_state_tw.select(Some(0));
                        app.table_state_us.select(Some(0));
                    }
                    Action::Sort(column) => {
                        app.toggle_sort(column);
                    }
                    Action::ToggleLive => {
                        app.live_mode = !app.live_mode;
                        if app.live_mode {
                            app.last_live_refresh = Instant::now();
                        }
                    }
                    Action::ToggleHide => {
                        app.hide_positions = !app.hide_positions;
                    }
                    Action::SelectTwRow(idx) => {
                        app.active_section = 0;
                        app.table_state_tw.select(Some(idx));
                    }
                    Action::SelectUsRow(idx) => {
                        app.active_section = 1;
                        app.table_state_us.select(Some(idx));
                    }
                    Action::ViewCombined => {
                        app.view_combined = true;
                        app.table_state_tw.select(Some(0));
                        app.table_state_us.select(Some(0));
                    }
                    Action::OpenDetail => {
                        if let Some(stock) = app.get_selected_stock() {
                            let symbol = stock.symbol.clone();
                            let historical = app.fetch_historical(&symbol);
                            // Update historical data in all vectors
                            for s in app.stocks.iter_mut().chain(app.tw_stocks.iter_mut())
                                .chain(app.us_stocks.iter_mut()).chain(app.combined_stocks.iter_mut())
                                .chain(app.combined_tw_stocks.iter_mut()).chain(app.combined_us_stocks.iter_mut())
                            {
                                if s.symbol == symbol {
                                    s.historical = historical.clone();
                                }
                            }
                            app.input_mode = InputMode::DetailView(symbol);
                        }
                    }
                    Action::None => {}
                }
        }
    }
}

fn handle_input(app: &mut App, key: KeyCode) -> Action {
    match &mut app.input_mode {
        InputMode::Normal => match key {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('0') => {
                app.view_combined = true;
                app.table_state_tw.select(Some(0));
                app.table_state_us.select(Some(0));
                Action::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap() as usize - 1;
                if idx < app.portfolios.len() {
                    Action::SwitchPortfolio(idx)
                } else {
                    Action::None
                }
            }
            KeyCode::Tab => {
                app.active_section = (app.active_section + 1) % 2;
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.next_row();
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.prev_row();
                Action::None
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if !app.view_combined && app.portfolios.len() > 1 {
                    let idx = (app.current_portfolio_idx + 1) % app.portfolios.len();
                    Action::SwitchPortfolio(idx)
                } else {
                    Action::None
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if !app.view_combined && app.portfolios.len() > 1 {
                    let idx = if app.current_portfolio_idx == 0 {
                        app.portfolios.len() - 1
                    } else {
                        app.current_portfolio_idx - 1
                    };
                    Action::SwitchPortfolio(idx)
                } else {
                    Action::None
                }
            }
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('a') if !app.view_combined => {
                app.input_mode = InputMode::AddStock(AddStockState::default());
                Action::None
            }
            KeyCode::Char('e') if !app.view_combined => {
                if let Some(stock) = app.get_selected_stock() {
                    app.input_mode = InputMode::EditStock(EditStockState {
                        symbol: stock.symbol.clone(),
                        quantity: stock.quantity.to_string(),
                        cost_basis: stock.cost_basis.to_string(),
                    });
                }
                Action::None
            }
            KeyCode::Char('d') if !app.view_combined => {
                if let Some(stock) = app.get_selected_stock() {
                    app.input_mode = InputMode::DeleteConfirm(stock.symbol.clone());
                }
                Action::None
            }
            KeyCode::Char('n') => {
                app.input_mode = InputMode::NewPortfolio(String::new());
                Action::None
            }
            // Sorting keys: F1/p=Price, F2/c=Change, F3/y=Qty, F4/g=Gain, F5/G=Gain%
            KeyCode::F(1) | KeyCode::Char('p') => Action::Sort(SortColumn::Price),
            KeyCode::F(2) | KeyCode::Char('c') => Action::Sort(SortColumn::Change),
            KeyCode::F(3) | KeyCode::Char('y') => Action::Sort(SortColumn::Quantity),
            KeyCode::F(4) | KeyCode::Char('g') => Action::Sort(SortColumn::Gain),
            KeyCode::F(5) | KeyCode::Char('G') => Action::Sort(SortColumn::GainPercent),
            // Toggle hide positions for privacy
            KeyCode::Char('H') => {
                app.hide_positions = !app.hide_positions;
                Action::None
            }
            // Toggle live mode (auto-refresh every 5 seconds)
            KeyCode::Char('L') => {
                app.live_mode = !app.live_mode;
                if app.live_mode {
                    app.last_live_refresh = Instant::now();
                }
                Action::None
            }
            // Toggle between gain amount and percentage in table titles
            KeyCode::Char('T') => {
                app.show_gain_amount = !app.show_gain_amount;
                Action::None
            }
            // Enter to view stock detail - fetch historical on demand
            KeyCode::Enter => {
                if let Some(stock) = app.get_selected_stock() {
                    let symbol = stock.symbol.clone();

                    // Fetch historical on-demand for chart
                    let historical = app.fetch_historical(&symbol);

                    // Update the stock's historical data in all vectors
                    for s in app.stocks.iter_mut() {
                        if s.symbol == symbol {
                            s.historical = historical.clone();
                        }
                    }
                    for s in app.tw_stocks.iter_mut() {
                        if s.symbol == symbol {
                            s.historical = historical.clone();
                        }
                    }
                    for s in app.us_stocks.iter_mut() {
                        if s.symbol == symbol {
                            s.historical = historical.clone();
                        }
                    }
                    for s in app.combined_stocks.iter_mut() {
                        if s.symbol == symbol {
                            s.historical = historical.clone();
                        }
                    }
                    for s in app.combined_tw_stocks.iter_mut() {
                        if s.symbol == symbol {
                            s.historical = historical.clone();
                        }
                    }
                    for s in app.combined_us_stocks.iter_mut() {
                        if s.symbol == symbol {
                            s.historical = historical.clone();
                        }
                    }

                    app.input_mode = InputMode::DetailView(symbol);
                }
                Action::None
            }
            _ => Action::None,
        },
        InputMode::DetailView(_) => match key {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                app.input_mode = InputMode::Normal;
                Action::None
            }
            _ => Action::None,
        },
        InputMode::AddStock(state) => match key {
            KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
                Action::None
            }
            KeyCode::Enter => {
                if state.step < 4 {
                    state.step += 1;
                    Action::None
                } else {
                    let mut symbol = state.symbol.trim().to_uppercase();
                    if symbol.chars().all(|c| c.is_ascii_digit()) && symbol.len() >= 4 && symbol.len() <= 6 {
                        symbol = format!("{}.TW", symbol);
                    }
                    let display = if state.display.is_empty() {
                        symbol.replace(".TW", "")
                    } else {
                        state.display.clone()
                    };
                    let name = if state.name.is_empty() {
                        symbol.clone()
                    } else {
                        state.name.clone()
                    };
                    let quantity: f64 = state.quantity.parse().unwrap_or(0.0);
                    let cost_basis: f64 = state.cost_basis.parse().unwrap_or(0.0);
                    Action::AddStock(symbol, display, name, quantity, cost_basis)
                }
            }
            KeyCode::Backspace => {
                let field = match state.step {
                    0 => &mut state.symbol,
                    1 => &mut state.display,
                    2 => &mut state.name,
                    3 => &mut state.quantity,
                    _ => &mut state.cost_basis,
                };
                field.pop();
                Action::None
            }
            KeyCode::Char(c) => {
                let field = match state.step {
                    0 => &mut state.symbol,
                    1 => &mut state.display,
                    2 => &mut state.name,
                    3 => &mut state.quantity,
                    _ => &mut state.cost_basis,
                };
                field.push(c);
                Action::None
            }
            _ => Action::None,
        },
        InputMode::EditStock(state) => match key {
            KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
                Action::None
            }
            KeyCode::Enter => {
                let symbol = state.symbol.clone();
                let quantity: f64 = state.quantity.parse().unwrap_or(0.0);
                let cost_basis: f64 = state.cost_basis.parse().unwrap_or(0.0);
                Action::EditStock(symbol, quantity, cost_basis)
            }
            KeyCode::Backspace => {
                state.quantity.pop();
                Action::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                state.quantity.push(c);
                Action::None
            }
            _ => Action::None,
        },
        InputMode::DeleteConfirm(symbol) => match key {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                Action::DeleteStock(symbol.clone())
            }
            _ => {
                app.input_mode = InputMode::Normal;
                Action::None
            }
        },
        InputMode::NewPortfolio(name) => match key {
            KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
                Action::None
            }
            KeyCode::Enter => {
                if !name.is_empty() {
                    Action::CreatePortfolio(name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Backspace => {
                name.pop();
                Action::None
            }
            KeyCode::Char(c) if c.is_alphanumeric() || c == '_' => {
                name.push(c.to_ascii_lowercase());
                Action::None
            }
            _ => Action::None,
        },
    }
}

/// Check if a point (x, y) is inside a Rect
fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn handle_mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16) -> Action {
    // Only handle left clicks
    let is_click = matches!(kind, MouseEventKind::Down(MouseButton::Left));

    if !is_click {
        return Action::None;
    }

    // In detail view, any click closes it
    if matches!(app.input_mode, InputMode::DetailView(_)) {
        app.input_mode = InputMode::Normal;
        return Action::None;
    }

    // Only handle mouse in Normal mode
    if !matches!(app.input_mode, InputMode::Normal) {
        return Action::None;
    }

    let regions = &app.clickable_regions;

    // Check portfolio tabs
    for (rect, idx) in &regions.portfolio_tabs {
        if point_in_rect(x, y, *rect) {
            if *idx == 0 {
                return Action::ViewCombined;
            } else {
                return Action::SwitchPortfolio(*idx - 1);
            }
        }
    }

    // Check TW stock table rows
    // Click on already-selected row opens detail view
    for (rect, row_idx) in &regions.tw_rows {
        if point_in_rect(x, y, *rect) {
            let currently_selected = app.table_state_tw.selected() == Some(*row_idx) && app.active_section == 0;
            if currently_selected {
                return Action::OpenDetail;
            }
            return Action::SelectTwRow(*row_idx);
        }
    }

    // Check US stock table rows
    for (rect, row_idx) in &regions.us_rows {
        if point_in_rect(x, y, *rect) {
            let currently_selected = app.table_state_us.selected() == Some(*row_idx) && app.active_section == 1;
            if currently_selected {
                return Action::OpenDetail;
            }
            return Action::SelectUsRow(*row_idx);
        }
    }

    // Check footer buttons
    for (rect, action_name) in &regions.footer_buttons {
        if point_in_rect(x, y, *rect) {
            return match *action_name {
                "live" => Action::ToggleLive,
                "hide" => Action::ToggleHide,
                "refresh" => Action::Refresh,
                "quit" => Action::Quit,
                _ => Action::None,
            };
        }
    }

    // Click on table area but not on a row - activate that section
    if point_in_rect(x, y, regions.tw_table) {
        app.active_section = 0;
    } else if point_in_rect(x, y, regions.us_table) {
        app.active_section = 1;
    }

    Action::None
}

fn ui(f: &mut Frame, app: &mut App) {
    // Clear clickable regions before each render
    app.clickable_regions = ClickableRegions::default();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Tabs
            Constraint::Min(10),    // Main content
            Constraint::Length(8),  // Summary
            Constraint::Length(2),  // Footer
        ])
        .split(f.area());

    render_tabs(f, app, chunks[0]);
    render_stock_tables(f, app, chunks[1]);
    render_summary(f, app, chunks[2]);
    render_footer(f, app, chunks[3]);

    // Render dialogs
    match &app.input_mode {
        InputMode::AddStock(state) => render_add_dialog(f, state),
        InputMode::EditStock(state) => render_edit_dialog(f, state),
        InputMode::DeleteConfirm(symbol) => render_delete_dialog(f, symbol),
        InputMode::NewPortfolio(name) => render_new_portfolio_dialog(f, name),
        InputMode::DetailView(symbol) => render_detail_view(f, app, symbol),
        InputMode::Normal => {}
    }
}

fn render_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let mut titles: Vec<Line> = vec![
        if app.view_combined {
            Line::from(" 0:ALL ").magenta().bold()
        } else {
            Line::from(" 0:ALL ").dark_gray()
        }
    ];

    // Track tab widths for click detection
    let mut tab_widths: Vec<usize> = vec![7]; // " 0:ALL " = 7 chars

    for (i, p) in app.portfolios.iter().enumerate() {
        let title = format!(" {}:{} ", i + 1, p.name);
        tab_widths.push(title.len());
        if !app.view_combined && i == app.current_portfolio_idx {
            titles.push(Line::from(title).cyan().bold());
        } else {
            titles.push(Line::from(title).dark_gray());
        }
    }

    // Calculate clickable regions for tabs (inside the border)
    let inner_x = area.x + 1; // Account for left border
    let tab_y = area.y + 1;   // Account for top border
    let mut current_x = inner_x;

    for (i, width) in tab_widths.iter().enumerate() {
        let tab_rect = Rect::new(current_x, tab_y, *width as u16, 1);
        app.clickable_regions.portfolio_tabs.push((tab_rect, i));
        current_x += *width as u16 + 1; // +1 for divider "|"
    }

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(" Portfolios "))
        .divider("|");

    f.render_widget(tabs, area);
}

fn render_stock_tables(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Record table areas for click detection
    app.clickable_regions.tw_table = chunks[0];
    app.clickable_regions.us_table = chunks[1];

    // Get stock counts first to avoid borrow issues
    let tw_count = if app.view_combined { app.combined_tw_stocks.len() } else { app.tw_stocks.len() };
    let us_count = if app.view_combined { app.combined_us_stocks.len() } else { app.us_stocks.len() };

    // Calculate row regions (rows start after border + header)
    let tw_row_start_y = chunks[0].y + 2; // +1 border, +1 header
    let tw_row_width = chunks[0].width.saturating_sub(2); // -2 for borders
    let tw_row_x = chunks[0].x + 1;
    for i in 0..tw_count {
        let row_y = tw_row_start_y + i as u16;
        if row_y < chunks[0].y + chunks[0].height - 1 { // Don't exceed table bounds
            let row_rect = Rect::new(tw_row_x, row_y, tw_row_width, 1);
            app.clickable_regions.tw_rows.push((row_rect, i));
        }
    }

    let us_row_start_y = chunks[1].y + 2;
    let us_row_width = chunks[1].width.saturating_sub(2);
    let us_row_x = chunks[1].x + 1;
    for i in 0..us_count {
        let row_y = us_row_start_y + i as u16;
        if row_y < chunks[1].y + chunks[1].height - 1 {
            let row_rect = Rect::new(us_row_x, row_y, us_row_width, 1);
            app.clickable_regions.us_rows.push((row_rect, i));
        }
    }

    let tw_stocks = app.get_active_tw_stocks();
    let us_stocks = app.get_active_us_stocks();

    // Sort indicator
    let sort_arrow = match app.sort_direction {
        SortDirection::Ascending => "▲",
        SortDirection::Descending => "▼",
    };

    let header_col = |name: &str, col: Option<SortColumn>| -> String {
        if app.sort_column == col {
            format!("{}{}", name, sort_arrow)
        } else {
            name.to_string()
        }
    };

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    // Build header based on hide_positions state
    let header = if app.hide_positions {
        let mut cols = vec![
            "Symbol".to_string(),
            "Name".to_string(),
            header_col("Price", Some(SortColumn::Price)),
            header_col("Change", Some(SortColumn::Change)),
        ];
        if app.view_combined {
            cols.push("Portfolio".to_string());
        }
        Row::new(cols).style(header_style).height(1)
    } else if app.view_combined {
        Row::new(vec![
            "Symbol".to_string(),
            "Name".to_string(),
            header_col("Price", Some(SortColumn::Price)),
            header_col("Change", Some(SortColumn::Change)),
            header_col("Qty", Some(SortColumn::Quantity)),
            "Cost".to_string(),
            header_col("Gain", Some(SortColumn::Gain)),
            header_col("Gain %", Some(SortColumn::GainPercent)),
            "Portfolio".to_string(),
        ])
            .style(header_style)
            .height(1)
    } else {
        Row::new(vec![
            "Symbol".to_string(),
            "Name".to_string(),
            header_col("Price", Some(SortColumn::Price)),
            header_col("Change", Some(SortColumn::Change)),
            header_col("Qty", Some(SortColumn::Quantity)),
            "Cost".to_string(),
            header_col("Gain", Some(SortColumn::Gain)),
            header_col("Gain %", Some(SortColumn::GainPercent)),
        ])
            .style(header_style)
            .height(1)
    };

    // Calculate market totals for titles
    let (tw_value, tw_gain, tw_gain_pct, us_value, us_gain, us_gain_pct) = app.calculate_market_summary();
    let tw_gain_color = if tw_gain >= 0.0 { Color::Green } else { Color::Red };
    let us_gain_color = if us_gain >= 0.0 { Color::Green } else { Color::Red };

    // TW Stocks
    let tw_base = if app.view_combined { "Taiwan Stocks (All)" } else { "Taiwan Stocks" };
    let tw_title: Line = if app.hide_positions {
        Line::from(tw_base)
    } else {
        let tw_gain_display = if app.show_gain_amount {
            format!("{:+.0} TWD", tw_gain)
        } else {
            format!("{:+.2}%", tw_gain_pct)
        };
        Line::from(vec![
            Span::raw(format!("{} ", tw_base)),
            Span::styled(format!("{:.0} TWD ", tw_value), Style::default().fg(Color::White)),
            Span::styled(tw_gain_display, Style::default().fg(tw_gain_color)),
        ])
    };
    let tw_rows: Vec<Row> = tw_stocks.iter().map(|s| stock_to_row(s, app.usd_twd_rate, app.view_combined, app.hide_positions)).collect();
    let tw_table = Table::new(tw_rows, get_widths(app.view_combined, app.hide_positions))
        .header(header.clone())
        .block(Block::default().borders(Borders::ALL).title(tw_title)
            .border_style(if app.active_section == 0 { Style::default().fg(Color::Cyan) } else { Style::default() }))
        .row_highlight_style(Style::default().bg(Color::DarkGray));

    f.render_stateful_widget(tw_table, chunks[0], &mut app.table_state_tw.clone());

    // US Stocks
    let us_base = if app.view_combined { "US Stocks (All)" } else { "US Stocks" };
    let us_title: Line = if app.hide_positions {
        Line::from(us_base)
    } else {
        let us_gain_display = if app.show_gain_amount {
            format!("{:+.2} USD", us_gain)
        } else {
            format!("{:+.2}%", us_gain_pct)
        };
        Line::from(vec![
            Span::raw(format!("{} ", us_base)),
            Span::styled(format!("{:.2} USD ", us_value), Style::default().fg(Color::White)),
            Span::styled(us_gain_display, Style::default().fg(us_gain_color)),
        ])
    };
    let us_rows: Vec<Row> = us_stocks.iter().map(|s| stock_to_row(s, app.usd_twd_rate, app.view_combined, app.hide_positions)).collect();
    let us_table = Table::new(us_rows, get_widths(app.view_combined, app.hide_positions))
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(us_title)
            .border_style(if app.active_section == 1 { Style::default().fg(Color::Cyan) } else { Style::default() }))
        .row_highlight_style(Style::default().bg(Color::DarkGray));

    f.render_stateful_widget(us_table, chunks[1], &mut app.table_state_us.clone());
}

fn get_widths(combined: bool, hide_positions: bool) -> Vec<Constraint> {
    if hide_positions {
        let mut widths = vec![
            Constraint::Length(10),  // Symbol
            Constraint::Length(16),  // Name
            Constraint::Length(12),  // Price
            Constraint::Length(10),  // Change
        ];
        if combined {
            widths.push(Constraint::Length(12));  // Portfolio
        }
        widths
    } else if combined {
        vec![
            Constraint::Length(8),   // Symbol
            Constraint::Length(10),  // Name
            Constraint::Length(10),  // Price
            Constraint::Length(9),   // Change
            Constraint::Length(8),   // Qty
            Constraint::Length(8),   // Cost
            Constraint::Length(12),  // Gain
            Constraint::Length(8),   // Gain %
            Constraint::Length(10),  // Portfolio
        ]
    } else {
        vec![
            Constraint::Length(8),   // Symbol
            Constraint::Length(12),  // Name
            Constraint::Length(10),  // Price
            Constraint::Length(9),   // Change
            Constraint::Length(8),   // Qty
            Constraint::Length(8),   // Cost
            Constraint::Length(12),  // Gain
            Constraint::Length(8),   // Gain %
        ]
    }
}

fn stock_to_row(stock: &Stock, usd_twd_rate: f64, show_portfolio: bool, hide_positions: bool) -> Row<'static> {
    let (price, change_pct) = stock.price_data.as_ref()
        .map(|d| (d.price, d.change_percent))
        .unwrap_or((0.0, 0.0));

    let arrow = if change_pct >= 0.0 { "↑" } else { "↓" };
    let color = if change_pct >= 0.0 { Color::Green } else { Color::Red };

    let mut cells = vec![
        Cell::from(stock.display.clone()),
        Cell::from(if show_portfolio { stock.name.chars().take(8).collect::<String>() } else { stock.name.chars().take(10).collect::<String>() }),
        Cell::from(Line::from(format!("{:.2}", price)).alignment(Alignment::Right)).style(Style::default().fg(color)),
        Cell::from(Line::from(format!("{}{:.1}%", arrow, change_pct)).alignment(Alignment::Right)).style(Style::default().fg(color)),
    ];

    // Only show position columns if not hidden
    if !hide_positions {
        let is_tw = stock.symbol.contains(".TW");
        let (gain, gain_pct) = if stock.quantity > 0.0 && stock.cost_basis > 0.0 {
            let current_value = stock.quantity * price;
            let cost_value = stock.quantity * stock.cost_basis;
            let mut gain = current_value - cost_value;
            if !is_tw {
                gain *= usd_twd_rate;
            }
            let pct = (gain / (cost_value * if is_tw { 1.0 } else { usd_twd_rate })) * 100.0;
            (gain, pct)
        } else {
            (0.0, 0.0)
        };

        let gain_color = if gain >= 0.0 { Color::Green } else { Color::Red };
        let gain_str = format!("{:+.0}", gain);
        let gain_pct_str = format!("{:+.1}%", gain_pct);

        cells.push(Cell::from(Line::from(format!("{:.0}", stock.quantity)).alignment(Alignment::Right)));
        cells.push(Cell::from(Line::from(format!("{:.1}", stock.cost_basis)).alignment(Alignment::Right)));
        cells.push(Cell::from(Line::from(gain_str).alignment(Alignment::Right)).style(Style::default().fg(gain_color)));
        cells.push(Cell::from(Line::from(gain_pct_str).alignment(Alignment::Right)).style(Style::default().fg(gain_color)));
    }

    if show_portfolio {
        cells.push(Cell::from(stock.portfolio_name.clone()).style(Style::default().fg(Color::DarkGray)));
    }

    Row::new(cells)
}

fn render_summary(f: &mut Frame, app: &App, area: Rect) {
    let title = if app.view_combined {
        " Combined Summary (All Portfolios) "
    } else {
        " Summary "
    };

    let time_str = Local::now().format("%H:%M:%S").to_string();

    // Status indicator: refreshing, live mode countdown, or nothing
    let status_indicator = if app.is_fetching {
        "  |  Refreshing...".to_string()
    } else if app.live_mode {
        let elapsed = app.last_live_refresh.elapsed().as_secs();
        let remaining = LIVE_REFRESH_INTERVAL_SECS.saturating_sub(elapsed);
        format!("  |  LIVE ({}s)", remaining)
    } else {
        String::new()
    };

    let status_color = if app.is_fetching { Color::Yellow } else { Color::Green };

    let text = if app.hide_positions {
        // Show minimal info when positions are hidden
        vec![
            Line::from(vec![
                Span::styled(format!("Updated: {}  |  USD/TWD: {:.2}", time_str, app.usd_twd_rate), Style::default().fg(Color::DarkGray)),
                Span::styled(status_indicator.clone(), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Positions hidden (press H to show)", Style::default().fg(Color::Yellow)),
            ]),
        ]
    } else {
        let (total_cost, total_value, total_gain, total_gain_percent, stock_count, holdings) = app.calculate_summary();
        let gain_color = if total_gain >= 0.0 { Color::Green } else { Color::Red };

        vec![
            Line::from(vec![
                Span::styled(format!("Updated: {}  |  USD/TWD: {:.2}", time_str, app.usd_twd_rate), Style::default().fg(Color::DarkGray)),
                Span::styled(status_indicator, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
            Line::from(format!("  Total Cost:   {:>15.2} TWD", total_cost)),
            Line::from(format!("  Total Value:  {:>15.2} TWD", total_value)),
            Line::from(vec![
                Span::raw("  Total Gain:   "),
                Span::styled(format!("{:>15.2} TWD ({:+.2}%)", total_gain, total_gain_percent), Style::default().fg(gain_color)),
            ]),
            Line::from(format!("  Stocks: {}  |  Holdings: {}", stock_count, holdings)),
        ]
    };

    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title)
            .title_style(if app.view_combined { Style::default().fg(Color::Magenta).bold() } else { Style::default() }));

    f.render_widget(paragraph, area);
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let hide_key = if app.hide_positions { "H=Show" } else { "H=Hide" };
    let live_key = if app.live_mode { "L=Live:ON" } else { "L=Live" };
    let title_key = if app.show_gain_amount { "T=$" } else { "T=%" };

    let base_keys = format!(" 0-9=Portfolio | ↑↓jk=Nav | Enter=Detail | Sort:pcygG | a=Add e=Edit d=Del | {} {} | ", hide_key, title_key);

    // Calculate button positions for click detection
    let base_len = base_keys.len() as u16;
    let live_len = live_key.len() as u16;

    // Hide button position (find "H=Show" or "H=Hide" in base_keys)
    if let Some(hide_pos) = base_keys.find(hide_key) {
        let hide_rect = Rect::new(area.x + hide_pos as u16, area.y, hide_key.len() as u16, 1);
        app.clickable_regions.footer_buttons.push((hide_rect, "hide"));
    }

    // Live button position (after base_keys)
    let live_rect = Rect::new(area.x + base_len, area.y, live_len, 1);
    app.clickable_regions.footer_buttons.push((live_rect, "live"));

    // Refresh button position
    let refresh_start = base_len + live_len + 3; // " | " = 3 chars
    let refresh_rect = Rect::new(area.x + refresh_start, area.y, 9, 1); // "r=Refresh" = 9
    app.clickable_regions.footer_buttons.push((refresh_rect, "refresh"));

    // Quit button position
    let quit_start = refresh_start + 9 + 3; // "r=Refresh" + " | "
    let quit_rect = Rect::new(area.x + quit_start, area.y, 6, 1); // "q=Quit" = 6
    app.clickable_regions.footer_buttons.push((quit_rect, "quit"));

    let spans = if app.live_mode {
        vec![
            Span::styled(base_keys, Style::default().fg(Color::Yellow)),
            Span::styled(live_key, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" | r=Refresh | q=Quit ", Style::default().fg(Color::Yellow)),
        ]
    } else {
        vec![
            Span::styled(base_keys, Style::default().fg(Color::Yellow)),
            Span::styled(live_key, Style::default().fg(Color::Yellow)),
            Span::styled(" | r=Refresh | q=Quit ", Style::default().fg(Color::Yellow)),
        ]
    };

    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(paragraph, area);
}

fn render_add_dialog(f: &mut Frame, state: &AddStockState) {
    let area = centered_rect(50, 50, f.area());
    f.render_widget(Clear, area);

    let prompts = ["Symbol:", "Display name:", "Description:", "Quantity:", "Cost basis:"];
    let values = [&state.symbol, &state.display, &state.name, &state.quantity, &state.cost_basis];

    let mut lines: Vec<Line> = vec![Line::from(""), Line::from("  Taiwan stocks auto-detected (e.g., 2330 → 2330.TW)"), Line::from("")];

    for (i, (prompt, value)) in prompts.iter().zip(values.iter()).enumerate() {
        let style = if i == state.step {
            Style::default().fg(Color::Yellow).bold()
        } else if i < state.step {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let cursor = if i == state.step { "█" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", prompt), style),
            Span::styled(format!("{}{}", value, cursor), style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("  Press Enter to continue, Esc to cancel").style(Style::default().fg(Color::DarkGray)));

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Add Stock ").border_style(Style::default().fg(Color::Yellow)));

    f.render_widget(paragraph, area);
}

fn render_edit_dialog(f: &mut Frame, state: &EditStockState) {
    let area = centered_rect(40, 30, f.area());
    f.render_widget(Clear, area);

    let lines = vec![
        Line::from(""),
        Line::from(format!("  Editing: {}", state.symbol)),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Quantity: "),
            Span::styled(format!("{}█", state.quantity), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
        Line::from(format!("  Cost basis: {}", state.cost_basis)),
        Line::from(""),
        Line::from("  Enter=Save, Esc=Cancel").style(Style::default().fg(Color::DarkGray)),
    ];

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Edit Holdings ").border_style(Style::default().fg(Color::Cyan)));

    f.render_widget(paragraph, area);
}

fn render_delete_dialog(f: &mut Frame, symbol: &str) {
    let area = centered_rect(40, 20, f.area());
    f.render_widget(Clear, area);

    let lines = vec![
        Line::from(""),
        Line::from(format!("  Delete {}?", symbol)),
        Line::from(""),
        Line::from("  Press Y to confirm, any key to cancel").style(Style::default().fg(Color::DarkGray)),
    ];

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Confirm Delete ").border_style(Style::default().fg(Color::Red)));

    f.render_widget(paragraph, area);
}

fn render_new_portfolio_dialog(f: &mut Frame, name: &str) {
    let area = centered_rect(40, 20, f.area());
    f.render_widget(Clear, area);

    let lines = vec![
        Line::from(""),
        Line::from("  Enter portfolio name:"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{}█", name), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
        Line::from("  Enter=Create, Esc=Cancel").style(Style::default().fg(Color::DarkGray)),
    ];

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" New Portfolio ").border_style(Style::default().fg(Color::Magenta)));

    f.render_widget(paragraph, area);
}

fn render_detail_view(f: &mut Frame, app: &App, symbol: &str) {
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);

    // Find the stock in all vectors
    let stock = app.tw_stocks.iter()
        .chain(app.us_stocks.iter())
        .chain(app.combined_tw_stocks.iter())
        .chain(app.combined_us_stocks.iter())
        .find(|s| s.symbol == symbol);

    let Some(stock) = stock else {
        let paragraph = Paragraph::new("Stock not found")
            .block(Block::default().borders(Borders::ALL).title(" Detail View "));
        f.render_widget(paragraph, area);
        return;
    };

    // Split area into sections
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // Info header
            Constraint::Min(10),    // Chart
            Constraint::Length(2),  // Footer
        ])
        .margin(1)
        .split(area);

    // Render border
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} - {} ", stock.display, stock.name))
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(block, area);

    // Info section
    let (price, change_pct) = stock.price_data.as_ref()
        .map(|d| (d.price, d.change_percent))
        .unwrap_or((0.0, 0.0));

    let price_color = if change_pct >= 0.0 { Color::Green } else { Color::Red };
    let arrow = if change_pct >= 0.0 { "↑" } else { "↓" };

    // Calculate 30-day high/low/avg from historical
    let (high, low, avg, trend_str) = stock.historical.as_ref()
        .map(|h| {
            let closes = &h.closes;
            let high = closes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let low = closes.iter().cloned().fold(f64::INFINITY, f64::min);
            let avg = closes.iter().sum::<f64>() / closes.len() as f64;
            let (trend, _) = App::calculate_trend(closes);
            (high, low, avg, trend.to_string())
        })
        .unwrap_or((0.0, 0.0, 0.0, "·".to_string()));

    let info_text = vec![
        Line::from(vec![
            Span::raw("  Current: "),
            Span::styled(format!("{:.2}", price), Style::default().fg(price_color).bold()),
            Span::raw("  "),
            Span::styled(format!("{}{:.2}%", arrow, change_pct), Style::default().fg(price_color)),
            Span::raw(format!("  |  30d Trend: {}", trend_str)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  30-Day High: {:.2}", high), Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            Span::styled(format!("Low: {:.2}", low), Style::default().fg(Color::Red)),
            Span::raw("  |  "),
            Span::raw(format!("Avg: {:.2}", avg)),
        ]),
    ];
    let info_para = Paragraph::new(info_text);
    f.render_widget(info_para, chunks[0]);

    // Chart section
    if let Some(historical) = &stock.historical {
        let closes = &historical.closes;
        if !closes.is_empty() {
            // Create chart data points: (x, y) where x is day index
            let data: Vec<(f64, f64)> = closes.iter()
                .enumerate()
                .map(|(i, &p)| (i as f64, p))
                .collect();

            let min_y = closes.iter().cloned().fold(f64::INFINITY, f64::min) * 0.98;
            let max_y = closes.iter().cloned().fold(f64::NEG_INFINITY, f64::max) * 1.02;
            let max_x = closes.len() as f64;

            let datasets = vec![
                Dataset::default()
                    .name("Price")
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(Color::Cyan))
                    .data(&data),
            ];

            let chart = Chart::new(datasets)
                .block(Block::default().borders(Borders::ALL).title(" 30-Day Price History "))
                .x_axis(
                    Axis::default()
                        .title("Days")
                        .style(Style::default().fg(Color::Gray))
                        .bounds([0.0, max_x])
                        .labels(vec![
                            Span::raw("30d ago"),
                            Span::raw("Today"),
                        ]),
                )
                .y_axis(
                    Axis::default()
                        .title("Price")
                        .style(Style::default().fg(Color::Gray))
                        .bounds([min_y, max_y])
                        .labels(vec![
                            Span::raw(format!("{:.1}", min_y)),
                            Span::raw(format!("{:.1}", max_y)),
                        ]),
                );

            f.render_widget(chart, chunks[1]);
        }
    } else {
        let no_data = Paragraph::new("  No historical data available")
            .block(Block::default().borders(Borders::ALL).title(" 30-Day Price History "))
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(no_data, chunks[1]);
    }

    // Footer
    let footer = Paragraph::new("  Press Esc or Enter to close")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
