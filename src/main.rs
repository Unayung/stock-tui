use anyhow::Result;
use chrono::Local;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs},
    Frame, Terminal,
};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

const CACHE_DURATION_SECS: u64 = 60;

#[derive(Clone, Debug)]
struct Stock {
    symbol: String,
    display: String,
    name: String,
    quantity: f64,
    cost_basis: f64,
    price_data: Option<PriceData>,
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
    sort_column: Option<SortColumn>,
    sort_direction: SortDirection,
    hide_positions: bool, // Toggle with 'h' to hide cost/quantity/gain for privacy
}

impl App {
    fn new() -> Result<Self> {
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
            sort_column: Some(SortColumn::Change), // Default sort by change %
            sort_direction: SortDirection::Descending,
            hide_positions: false,
        };
        app.load_portfolios()?;
        app.refresh_data()?;
        Ok(app)
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

        // Fetch from Yahoo Finance (blocking for simplicity)
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

    fn refresh_data(&mut self) -> Result<()> {
        self.usd_twd_rate = self.fetch_exchange_rate();

        // Load current portfolio stocks
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

        // Load combined stocks (aggregated) - sort_stocks() is called inside
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
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                let action = handle_input(app, key.code);

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
                        app.cache.clear();
                        app.refresh_data()?;
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
                    Action::None => {}
                }
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

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Tabs
            Constraint::Min(10),    // Main content
            Constraint::Length(7),  // Summary
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
        InputMode::Normal => {}
    }
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut titles: Vec<Line> = vec![
        if app.view_combined {
            Line::from(" 0:ALL ").magenta().bold()
        } else {
            Line::from(" 0:ALL ").dark_gray()
        }
    ];

    for (i, p) in app.portfolios.iter().enumerate() {
        let title = format!(" {}:{} ", i + 1, p.name);
        if !app.view_combined && i == app.current_portfolio_idx {
            titles.push(Line::from(title).cyan().bold());
        } else {
            titles.push(Line::from(title).dark_gray());
        }
    }

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(" Portfolios "))
        .divider("|");

    f.render_widget(tabs, area);
}

fn render_stock_tables(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

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

    // TW Stocks
    let tw_title = if app.view_combined { "Taiwan Stocks (All)" } else { "Taiwan Stocks" };
    let tw_rows: Vec<Row> = tw_stocks.iter().map(|s| stock_to_row(s, app.usd_twd_rate, app.view_combined, app.hide_positions)).collect();
    let tw_table = Table::new(tw_rows, get_widths(app.view_combined, app.hide_positions))
        .header(header.clone())
        .block(Block::default().borders(Borders::ALL).title(tw_title)
            .border_style(if app.active_section == 0 { Style::default().fg(Color::Cyan) } else { Style::default() }))
        .row_highlight_style(Style::default().bg(Color::DarkGray));

    f.render_stateful_widget(tw_table, chunks[0], &mut app.table_state_tw.clone());

    // US Stocks
    let us_title = if app.view_combined { "US Stocks (All)" } else { "US Stocks" };
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
        // Minimal columns: Symbol, Name, Price, Change, (Portfolio if combined)
        let mut widths = vec![
            Constraint::Length(10),  // Symbol
            Constraint::Length(20),  // Name (wider when fewer columns)
            Constraint::Length(12),  // Price
            Constraint::Length(12),  // Change
        ];
        if combined {
            widths.push(Constraint::Length(12));  // Portfolio
        }
        widths
    } else if combined {
        vec![
            Constraint::Length(10),  // Symbol
            Constraint::Length(12),  // Name
            Constraint::Length(12),  // Price
            Constraint::Length(10),  // Change
            Constraint::Length(10),  // Qty
            Constraint::Length(10),  // Cost
            Constraint::Length(14),  // Gain
            Constraint::Length(10),  // Gain %
            Constraint::Length(12),  // Portfolio
        ]
    } else {
        vec![
            Constraint::Length(10),  // Symbol
            Constraint::Length(14),  // Name
            Constraint::Length(12),  // Price
            Constraint::Length(10),  // Change
            Constraint::Length(10),  // Qty
            Constraint::Length(10),  // Cost
            Constraint::Length(14),  // Gain
            Constraint::Length(10),  // Gain %
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
        Cell::from(if show_portfolio { stock.name.chars().take(10).collect::<String>() } else { stock.name.clone() }),
        Cell::from(Line::from(format!("{:.2}", price)).alignment(Alignment::Right)).style(Style::default().fg(color)),
        Cell::from(Line::from(format!("{}{:.2}%", arrow, change_pct)).alignment(Alignment::Right)).style(Style::default().fg(color)),
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
        let gain_str = format!("{:+.2}", gain);
        let gain_pct_str = format!("{:+.1}%", gain_pct);

        cells.push(Cell::from(Line::from(format!("{:.2}", stock.quantity)).alignment(Alignment::Right)));
        cells.push(Cell::from(Line::from(format!("{:.2}", stock.cost_basis)).alignment(Alignment::Right)));
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

    let text = if app.hide_positions {
        // Show minimal info when positions are hidden
        vec![
            Line::from(vec![
                Span::styled(format!("Updated: {}  |  USD/TWD: {:.2}", time_str, app.usd_twd_rate), Style::default().fg(Color::DarkGray)),
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

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let hide_key = if app.hide_positions { "H=Show" } else { "H=Hide" };
    let keys = format!(" 0-9=Portfolio | Tab=Section | ↑↓=Nav | Sort: p/c/y/g/G | a=Add | e=Edit | d=Delete | n=New | {} | r=Refresh | q=Quit ", hide_key);
    let paragraph = Paragraph::new(keys)
        .style(Style::default().fg(Color::Yellow));
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
