use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use directories::BaseDirs;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fmt,
    io::{self, stdout},
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use strum::EnumIter;
use strum::IntoEnumIterator;
use tokio::sync::Notify;
use tracing::Level;
use tracing::{debug, info, warn};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, registry::Registry, Layer};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use winreg::{enums::*, RegKey};

const DEBOUNCE: Duration = Duration::from_millis(100);
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(200);

static KEY_COUNT: AtomicUsize = AtomicUsize::new(0);
static VALUE_COUNT: AtomicUsize = AtomicUsize::new(0);
static HKLM: RegKey = RegKey::predef(HKEY_LOCAL_MACHINE);

const REGEDIT_OUTPUT_FOR_BLANK_NAMES: bool = true;

pub struct WorkerManager {
    threads: usize,
    search_terms: Vec<String>,
    key_queue: Arc<Mutex<VecDeque<String>>>,
    work_ready_for_processing: Arc<Notify>,
    threads_waiting_for_work: Arc<AtomicUsize>,
    no_work_left: Arc<Notify>,
    pub results: Arc<Mutex<HashSet<String>>>,
    pub errors: Arc<Mutex<HashSet<String>>>,
}

impl WorkerManager {
    pub fn new(search_terms: Vec<String>, threads_to_use: usize) -> Self {
        Self {
            threads: threads_to_use,
            search_terms: search_terms
                .into_iter()
                .map(|term| term.to_lowercase())
                .collect(),
            key_queue: Arc::new(Mutex::new(VecDeque::new())),
            work_ready_for_processing: Arc::new(Notify::new()),
            threads_waiting_for_work: Arc::new(AtomicUsize::new(0)),

            no_work_left: Arc::new(Notify::new()),

            results: Arc::new(Mutex::new(HashSet::new())),
            errors: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn feed_queue_and_process_values(&self, key_path: &str) -> Result<(), Box<dyn Error>> {
        if self.string_matches(key_path) {
            self.results
                .lock()
                .insert(format!("HKEY_LOCAL_MACHINE\\{}", key_path));
        }
        let registry_key = HKLM.open_subkey_with_flags(key_path, KEY_READ)?;
        {
            let mut key_paths = Vec::new();
            for key_result in registry_key.enum_keys() {
                KEY_COUNT.fetch_add(1, Ordering::SeqCst);
                match key_result {
                    Ok(key_name) => {
                        let key_path = format!("{}\\{}", key_path, key_name);
                        key_paths.push(key_path);
                    }
                    Err(err) => {
                        self.errors
                            .lock()
                            .insert(format!("{}, Subkey error: \"{}\"", key_path, err));
                    }
                }
            }
            self.feed_queue(key_paths);
            self.work_ready_for_processing.notify_waiters();
        }

        for value_result in registry_key.enum_values() {
            VALUE_COUNT.fetch_add(1, Ordering::SeqCst);
            match value_result {
                Ok((value_name, reg_value)) => {
                    let data = reg_value.to_string();
                    if self.any_string_matches(&value_name, &data) {
                        let value_name = if value_name.is_empty() {
                            if REGEDIT_OUTPUT_FOR_BLANK_NAMES {
                                "(Default)".to_string()
                            } else {
                                value_name
                            }
                        } else {
                            value_name
                        };
                        self.results.lock().insert(format!(
                            "HKEY_LOCAL_MACHINE\\{}\\{} = \"{}\" ({:?})",
                            key_path, value_name, data, reg_value.vtype,
                        ));
                    }
                }
                Err(err) => {
                    self.errors
                        .lock()
                        .insert(format!("{}, Value error: \"{}\"", key_path, err));
                }
            }
        }
        Ok(())
    }

    pub async fn get_work(&self) -> Option<String> {
        loop {
            let work = self.key_queue.lock().pop_front();
            if let Some(key) = work {
                return Some(key);
            } else {
                self.threads_waiting_for_work.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    _ = self.work_ready_for_processing.notified() => {},
                    _ = self.no_work_left.notified() => return None,
                }
                self.threads_waiting_for_work.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    pub fn feed_queue(&self, keys: Vec<String>) {
        let mut lock = self.key_queue.lock();
        lock.extend(keys);
    }

    pub fn any_string_matches(&self, string: &str, string2: &str) -> bool {
        let string_lowercase = string.to_lowercase();
        let string2_lowercase = string2.to_lowercase();
        for term in self.search_terms.iter() {
            if string_lowercase.contains(term) || string2_lowercase.contains(term) {
                return true;
            }
        }
        false
    }

    pub fn string_matches(&self, string: &str) -> bool {
        let string_lowercase = string.to_lowercase();
        for term in self.search_terms.iter() {
            if string_lowercase.contains(term) {
                return true;
            }
        }
        false
    }

    pub async fn run(&self, worker_manager: Arc<WorkerManager>) {
        for _ in 0..worker_manager.threads {
            let worker_manager = worker_manager.to_owned();
            tokio::spawn(run_thread(worker_manager));
        }
        self.work_ready_for_processing.notify_waiters();
        loop {
            if worker_manager
                .threads_waiting_for_work
                .load(Ordering::SeqCst)
                == worker_manager.threads
            {
                if self.key_queue.lock().len() == 0 {
                    self.no_work_left.notify_waiters();
                    break;
                } else {
                    self.work_ready_for_processing.notify_waiters();
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

async fn run_thread(worker_manager: Arc<WorkerManager>) {
    loop {
        let key_path = match worker_manager.get_work().await {
            Some(key_path) => key_path,
            None => break,
        };
        if let Err(err) = worker_manager.feed_queue_and_process_values(&key_path) {
            worker_manager
                .errors
                .lock()
                .insert(format!("{}, Key error: \"{}\"", key_path, err));
        }
    }
}

#[derive(EnumIter)]
pub enum Root {
    HkeyClassesRoot = 0,
    HkeyCurrentUser = 1,
    HkeyLocalMachine = 2,
    HkeyUsers = 3,
    HkeyCurrentConfig = 4,
}

impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::HkeyClassesRoot => "HKEY_CLASSES_ROOT",
                Self::HkeyCurrentUser => "HKEY_CURRENT_USER",
                Self::HkeyLocalMachine => "HKEY_LOCAL_MACHINE",
                Self::HkeyUsers => "HKEY_USERS",
                Self::HkeyCurrentConfig => "HKEY_CURRENT_CONFIG",
            }
        )
    }
}

impl Root {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Root::HkeyClassesRoot),
            1 => Some(Root::HkeyCurrentUser),
            2 => Some(Root::HkeyLocalMachine),
            3 => Some(Root::HkeyUsers),
            4 => Some(Root::HkeyCurrentConfig),
            _ => None,
        }
    }
}

pub struct SelectedRoots {
    classes_root: bool,
    current_user: bool,
    local_machine: bool,
    users: bool,
    current_config: bool,
}

impl Default for SelectedRoots {
    fn default() -> Self {
        Self {
            classes_root: false,
            current_user: false,
            local_machine: true,
            users: true,
            current_config: false,
        }
    }
}

impl SelectedRoots {
    pub fn export_roots(&self) -> Vec<Root> {
        let mut selected_roots = Vec::new();

        if self.classes_root {
            selected_roots.push(Root::HkeyClassesRoot);
        }
        if self.current_user {
            selected_roots.push(Root::HkeyCurrentUser);
        }
        if self.local_machine {
            selected_roots.push(Root::HkeyLocalMachine);
        }
        if self.users {
            selected_roots.push(Root::HkeyUsers);
        }
        if self.current_config {
            selected_roots.push(Root::HkeyCurrentConfig);
        }

        selected_roots
    }

    pub fn is_selected(&self, root: &Root) -> bool {
        match root {
            Root::HkeyClassesRoot => self.classes_root,
            Root::HkeyCurrentUser => self.current_user,
            Root::HkeyLocalMachine => self.local_machine,
            Root::HkeyUsers => self.users,
            Root::HkeyCurrentConfig => self.current_config,
        }
    }

    pub fn toggle(&mut self, root: &Root) {
        match root {
            Root::HkeyClassesRoot => self.classes_root = !self.classes_root,
            Root::HkeyCurrentUser => self.current_user = !self.current_user,
            Root::HkeyLocalMachine => self.local_machine = !self.local_machine,
            Root::HkeyUsers => self.users = !self.users,
            Root::HkeyCurrentConfig => self.current_config = !self.current_config,
        }
    }
}

pub struct StaticSelection {
    pane_selected: Arc<AtomicU8>,
    pane_last_changed: Arc<Mutex<Instant>>,

    root_selected: Arc<AtomicU8>,
    root_selection_last_changed: Arc<Mutex<Instant>>,

    control_selected: Arc<AtomicU8>,

    selected_roots: Arc<RwLock<SelectedRoots>>,
}

impl Default for StaticSelection {
    fn default() -> Self {
        Self {
            pane_selected: Arc::new(AtomicU8::new(0)),
            pane_last_changed: Arc::new(Mutex::new(Instant::now())),
            root_selected: Arc::new(AtomicU8::new(0)),
            root_selection_last_changed: Arc::new(Mutex::new(Instant::now())),
            control_selected: Arc::new(AtomicU8::new(0)),
            selected_roots: Arc::new(RwLock::new(SelectedRoots::default())),
        }
    }
}

impl StaticSelection {
    pub fn generate_root_list(&self) -> Vec<Spans<'static>> {
        let root_selected = self.root_selected.load(Ordering::SeqCst);
        let pane_selected = self.pane_selected.load(Ordering::SeqCst) == 0;
        Root::iter()
            .map(|root| {
                Spans::from(Span::styled(
                    format!(
                        "{:25}{}",
                        root.to_string(),
                        if self.selected_roots.read().is_selected(&root) {
                            "Enabled"
                        } else {
                            "Disabled"
                        }
                    ),
                    Style::default().fg(if pane_selected && root as u8 == root_selected {
                        Color::Cyan
                    } else {
                        Color::White
                    }),
                ))
            })
            .collect::<Vec<Spans>>()
    }
    pub fn pane_left(&self) {
        if self.pane_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.pane_selected.load(Ordering::SeqCst) {
            0 => 2,
            1 => 0,
            2 => 1,
            _ => return,
        };
        self.pane_selected.store(new_value, Ordering::SeqCst);
        *self.pane_last_changed.lock() = Instant::now();
    }

    pub fn pane_right(&self) {
        if self.pane_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.pane_selected.load(Ordering::SeqCst) {
            0 => 1,
            1 => 2,
            2 => 0,
            _ => return,
        };
        self.pane_selected.store(new_value, Ordering::SeqCst);
        *self.pane_last_changed.lock() = Instant::now();
    }

    pub fn root_up(&self) {
        if self.root_selection_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.root_selected.load(Ordering::SeqCst) {
            0 => 4,
            1 => 0,
            2 => 1,
            3 => 2,
            4 => 3,
            _ => return,
        };
        self.root_selected.store(new_value, Ordering::SeqCst);
        *self.root_selection_last_changed.lock() = Instant::now();
    }

    pub fn root_down(&self) {
        if self.root_selection_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.root_selected.load(Ordering::SeqCst) {
            0 => 1,
            1 => 2,
            2 => 3,
            3 => 4,
            4 => 0,
            _ => return,
        };
        self.root_selected.store(new_value, Ordering::SeqCst);
        *self.root_selection_last_changed.lock() = Instant::now();
    }

    pub fn root_toggle(&self) {
        let selected = self.root_selected.load(Ordering::SeqCst);
        if let Some(root) = Root::from_u8(selected) {
            self.selected_roots.write().toggle(&root);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let base_directories = BaseDirs::new().expect("Base directories not found");
    let log_path = base_directories
        .config_dir()
        .join("windows_registry_search/logs/");
    let file = tracing_appender::rolling::daily(log_path, format!("log"));
    let (stdout_writer, _guard) = tracing_appender::non_blocking(stdout());
    let (file_writer, _guard) = tracing_appender::non_blocking(file);
    let logfile_layer = tracing_subscriber::fmt::layer().with_writer(file_writer);
    let level_filter = LevelFilter::from_level(Level::DEBUG);
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_writer(stdout_writer)
        .with_filter(level_filter);
    let subscriber = Registry::default().with(stdout_layer).with(logfile_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let static_menu_selection: Arc<StaticSelection> = Arc::new(StaticSelection::default());
    let static_menu_selection_event_receiver = static_menu_selection.to_owned();
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let stop_ = stop.to_owned();
    thread::spawn(move || loop {
        if event::poll(EVENT_POLL_TIMEOUT).unwrap() {
            if let Ok(CEvent::Key(key)) = event::read() {
                if let KeyEventKind::Press = key.kind {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            stop_.store(true, Ordering::SeqCst);
                            break;
                        }
                        KeyCode::Left => static_menu_selection_event_receiver.pane_left(),
                        KeyCode::Right => static_menu_selection_event_receiver.pane_right(),
                        KeyCode::Up => match static_menu_selection_event_receiver
                            .pane_selected
                            .load(Ordering::SeqCst)
                        {
                            0 => static_menu_selection_event_receiver.root_up(),
                            1 => {}
                            2 => {}
                            _ => {}
                        },
                        KeyCode::Down => match static_menu_selection_event_receiver
                            .pane_selected
                            .load(Ordering::SeqCst)
                        {
                            0 => static_menu_selection_event_receiver.root_down(),
                            1 => {}
                            2 => {}
                            _ => {}
                        },
                        KeyCode::Enter => match static_menu_selection_event_receiver
                            .pane_selected
                            .load(Ordering::SeqCst)
                        {
                            0 => static_menu_selection_event_receiver.root_toggle(),
                            1 => {}
                            2 => {}
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        } else {
        }
    });

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .margin(1)
                .constraints(
                    [
                        Constraint::Percentage(20), // Selection
                        Constraint::Percentage(20), // Controls
                        Constraint::Percentage(60), // Results
                    ]
                    .as_ref(),
                )
                .split(f.size());

            let pane_selected = static_menu_selection.pane_selected.load(Ordering::SeqCst);

            let left_paragraph = Paragraph::new(static_menu_selection.generate_root_list()).block(
                Block::default()
                    .title(Span::styled("Selection", Style::default().fg(Color::White)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 0 {
                        Color::LightMagenta
                    } else {
                        Color::White
                    })),
            );
            f.render_widget(left_paragraph, chunks[0]);

            let controls: Vec<Spans> = vec!["Start", "Stop", "Pause"]
                .iter()
                .map(|&control| {
                    Spans::from(Span::styled(control, Style::default().fg(Color::White)))
                })
                .collect();
            let middle_paragraph = Paragraph::new(controls)
                .block(
                    Block::default()
                        .title(Span::styled("Controls", Style::default().fg(Color::White)))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(if pane_selected == 1 {
                            Color::LightMagenta
                        } else {
                            Color::White
                        })),
                )
                .wrap(Wrap { trim: true });
            f.render_widget(middle_paragraph, chunks[1]);

            let right_text = Text::from("Results will be shown here.");
            let right_paragraph = Paragraph::new(right_text).block(
                Block::default()
                    .title(Span::styled("Results", Style::default().fg(Color::White)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 2 {
                        Color::LightMagenta
                    } else {
                        Color::White
                    })),
            );
            f.render_widget(right_paragraph, chunks[2]);
        })?;
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    /* let worker_manager = Arc::new(WorkerManager::new(vec!["Google Chrome".to_string(), "7-Zip".to_string()], num_cpus::get()));

    worker_manager.feed_queue(vec!["Software".to_string()]);
    let start_time = Instant::now();
    worker_manager.run(worker_manager.to_owned()).await;

    eprintln!("Errors:");
    for error in worker_manager.errors.lock().iter() {
        eprintln!("{}", error);
    }

    println!("\nResults:");
    for result in worker_manager.results.lock().iter() {
        println!("{}", result);
    }
    println!("Key count: {}, Value count: {}", KEY_COUNT.load(Ordering::SeqCst), VALUE_COUNT.load(Ordering::SeqCst));
    println!("Completed in {}ms.", start_time.elapsed().as_millis()); */
    Ok(())
}
