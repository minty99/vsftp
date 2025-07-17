use clap::Parser;
use color_eyre::eyre::{self, Result};
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::*;
use ssh2::{FileStat, Session};
use std::io::{Read, Write, stdout};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    remote: String,
    #[arg(default_value = ".")]
    path: String,
}

#[derive(Debug, Clone)]
enum DownloadProgress {
    Started,
    InProgress(u64, u64),
    Completed,
    Failed(String),
}

#[derive(Clone)]
enum FileListItem {
    Parent,
    File(String, FileStat),
    Directory(String),
}

struct AppState {
    exit: bool,
    current_path: PathBuf,
    items: Vec<FileListItem>,
    selected_item: ListState,
    logs: Vec<String>,
    download_status: Option<(String, DownloadProgress)>,
}

impl AppState {
    fn new(initial_path: String) -> Self {
        Self {
            exit: false,
            current_path: PathBuf::from(initial_path),
            items: Vec::new(),
            selected_item: ListState::default(),
            logs: vec!["App initialized".to_string()],
            download_status: None,
        }
    }

    fn log(&mut self, message: &str) {
        self.logs.push(message.to_string());
    }
}

struct App {
    sess: Session,
    state: Arc<Mutex<AppState>>,
    progress_receiver: mpsc::Receiver<(String, DownloadProgress)>,
    progress_sender: mpsc::Sender<(String, DownloadProgress)>,
}

impl App {
    fn new(sess: Session, initial_path: String) -> Self {
        let (tx, rx) = mpsc::channel(100);
        let mut app = Self {
            sess,
            state: Arc::new(Mutex::new(AppState::new(initial_path))),
            progress_receiver: rx,
            progress_sender: tx,
        };
        app.refresh_files();
        app
    }

    fn refresh_files(&mut self) {
        let mut state = self.state.lock().unwrap();
        let path_display = state.current_path.display().to_string();
        state.log(&format!("Fetching files from '{path_display}'..."));
        let sftp = self.sess.sftp().unwrap();
        match sftp.readdir(&state.current_path) {
            Ok(sftp_items) => {
                let mut items = Vec::new();
                // Add parent directory link if not in root
                if state.current_path != PathBuf::from(".")
                    && state.current_path != PathBuf::from("/")
                {
                    items.push(FileListItem::Parent);
                }

                let mut sorted_items: Vec<_> = sftp_items.into_iter().collect();
                sorted_items.sort_by(|(path_a, _), (path_b, _)| {
                    let a_is_dir = path_a.is_dir();
                    let b_is_dir = path_b.is_dir();
                    a_is_dir.cmp(&b_is_dir).reverse().then_with(|| {
                        path_a
                            .file_name()
                            .unwrap_or_default()
                            .cmp(path_b.file_name().unwrap_or_default())
                    })
                });

                for (path, stat) in sorted_items {
                    let name = path.file_name().unwrap().to_str().unwrap().to_string();
                    if stat.is_dir() {
                        items.push(FileListItem::Directory(name));
                    } else {
                        items.push(FileListItem::File(name, stat));
                    }
                }
                state.items = items;

                if !state.items.is_empty() {
                    state.selected_item.select(Some(0));
                }
                let count = state.items.len();
                state.log(&format!("Found {count} items."));
            }
            Err(e) => state.log(&format!("Error fetching files: {e}")),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        {
            let mut state = self.state.lock().unwrap();
            state.log(&format!("Key pressed: {key:?}"));
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                let mut state = self.state.lock().unwrap();
                state.exit = true;
            }
            KeyCode::Up => self.select_previous(),
            KeyCode::Down => self.select_next(),
            KeyCode::Enter => self.handle_enter(key.modifiers),
            _ => {}
        }
    }

    fn select_previous(&self) {
        let mut state = self.state.lock().unwrap();
        if state.items.is_empty() {
            return;
        }
        let i = match state.selected_item.selected() {
            Some(i) => {
                if i == 0 {
                    state.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        state.selected_item.select(Some(i));
    }

    fn select_next(&self) {
        let mut state = self.state.lock().unwrap();
        if state.items.is_empty() {
            return;
        }
        let i = match state.selected_item.selected() {
            Some(i) => {
                if i >= state.items.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        state.selected_item.select(Some(i));
    }

    fn handle_enter(&mut self, modifiers: KeyModifiers) {
        let selected_item = {
            let state = self.state.lock().unwrap();
            if let Some(selected_index) = state.selected_item.selected() {
                state.items[selected_index].clone()
            } else {
                return;
            }
        };

        match selected_item {
            FileListItem::Parent => {
                let mut state = self.state.lock().unwrap();
                state.current_path.pop();
                drop(state);
                self.refresh_files();
            }
            FileListItem::Directory(name) => {
                if modifiers.contains(KeyModifiers::CONTROL) {
                    // Use Ctrl+Enter for folder download
                    self.handle_folder_download(&name);
                } else {
                    let mut state = self.state.lock().unwrap();
                    state.current_path.push(name);
                    drop(state);
                    self.refresh_files();
                }
            }
            FileListItem::File(name, stat) => {
                let mut state = self.state.lock().unwrap();
                state.log(&format!("Starting download for '{name}'"));
                state.download_status = Some((name.clone(), DownloadProgress::Started));
                let sess_clone = self.sess.clone();
                let remote_path = state.current_path.join(&name);
                let progress_sender_clone = self.progress_sender.clone();

                tokio::spawn(async move {
                    let result = download_file(
                        sess_clone,
                        remote_path.clone(),
                        name.clone(),
                        stat,
                        progress_sender_clone.clone(),
                    )
                    .await;

                    if let Err(e) = result {
                        let _ = progress_sender_clone
                            .send((name.clone(), DownloadProgress::Failed(e.to_string())))
                            .await;
                    }
                });
            }
        }
    }

    fn handle_folder_download(&mut self, name: &str) {
        let mut state = self.state.lock().unwrap();
        state.log(&format!("Queueing directory '{name}' for download..."));
        let sess_clone = self.sess.clone();
        let current_path = state.current_path.clone();
        let progress_sender_clone = self.progress_sender.clone();
        let app_state_clone = self.state.clone();
        let name_owned = name.to_string();

        drop(state);

        tokio::spawn(async move {
            let remote_path = current_path.join(name_owned);
            let files_to_download =
                match find_files_recursive(sess_clone.clone(), remote_path.clone()).await {
                    Ok(files) => files,
                    Err(e) => {
                        let mut state = app_state_clone.lock().unwrap();
                        state.log(&format!("Error finding files in directory: {e}"));
                        return;
                    }
                };
            {
                let mut state = app_state_clone.lock().unwrap();
                state.log(&format!(
                    "Found {} files to download.",
                    files_to_download.len()
                ));
            }

            for (file_path, file_stat) in files_to_download {
                tokio::spawn(download_and_report_progress(
                    sess_clone.clone(),
                    app_state_clone.clone(),
                    progress_sender_clone.clone(),
                    file_path,
                    file_stat,
                ));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    async fn update_progress(&mut self) {
        if let Ok((name, progress)) = self.progress_receiver.try_recv() {
            let mut state = self.state.lock().unwrap();
            match progress {
                DownloadProgress::Completed => {
                    state.log(&format!("Download complete: {name}"));
                    state.download_status = None;
                }
                DownloadProgress::Failed(e) => {
                    state.log(&format!("Download failed for {name}: {e}"));
                    state.download_status = None;
                }
                _ => {
                    state.download_status = Some((name, progress));
                }
            }
        }
    }
}

fn find_files_recursive<'a>(
    sess: Session,
    path: PathBuf,
) -> futures::future::BoxFuture<'a, Result<Vec<(PathBuf, FileStat)>>> {
    Box::pin(async move {
        let sftp = sess.sftp()?;
        let mut files = Vec::new();
        let readdir_result = sftp.readdir(&path)?;

        for (item_path, stat) in readdir_result {
            if stat.is_dir() {
                let mut sub_files = find_files_recursive(sess.clone(), item_path).await?;
                files.append(&mut sub_files);
            } else {
                files.push((item_path, stat));
            }
        }
        Ok(files)
    })
}

async fn download_and_report_progress(
    sess: Session,
    app_state: Arc<Mutex<AppState>>,
    progress_sender: mpsc::Sender<(String, DownloadProgress)>,
    file_path: PathBuf,
    file_stat: FileStat,
) {
    let local_filename = file_path.file_name().unwrap().to_str().unwrap().to_string();
    {
        let mut state = app_state.lock().unwrap();
        state.log(&format!("Starting download for '{local_filename}'"));
        state.download_status = Some((local_filename.clone(), DownloadProgress::Started));
    }

    let download_result = download_file(
        sess.clone(),
        file_path.clone(),
        local_filename.clone(),
        file_stat,
        progress_sender.clone(),
    )
    .await;

    if let Err(e) = download_result {
        let _ = progress_sender
            .send((
                local_filename.clone(),
                DownloadProgress::Failed(e.to_string()),
            ))
            .await;
    }
}

async fn download_file(
    sess: Session,
    remote_path: PathBuf,
    local_filename: String,
    stat: FileStat,
    sender: mpsc::Sender<(String, DownloadProgress)>,
) -> Result<()> {
    let sftp = sess.sftp()?;
    let mut remote_file = sftp.open(&remote_path)?;
    let mut local_file = std::fs::File::create(&local_filename)?;
    let total_bytes = stat.size.unwrap_or(0);
    let mut downloaded_bytes = 0;
    let mut buffer = [0; 8192];

    loop {
        let bytes_read = remote_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        local_file.write_all(&buffer[..bytes_read])?;
        downloaded_bytes += bytes_read as u64;
        sender
            .send((
                local_filename.clone(),
                DownloadProgress::InProgress(downloaded_bytes, total_bytes),
            ))
            .await?;
    }

    sender
        .send((local_filename.clone(), DownloadProgress::Completed))
        .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();

    let (user, host) = args.remote.split_once('@').unwrap();
    let tcp = TcpStream::connect(format!("{host}:22"))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    let password = rpassword::prompt_password(format!("Password for {user}: "))?;
    sess.userauth_password(user, &password)?;
    if !sess.authenticated() {
        eyre::bail!("Authentication failed.");
    }

    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let mut app = App::new(sess, args.path);

    loop {
        {
            let mut state = app.state.lock().unwrap();
            if state.exit {
                break;
            }
            terminal.draw(|frame| ui(frame, &mut state))?;
        }

        app.update_progress().await;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                app.on_key(key);
            }
        }
    }

    stdout().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;

    Ok(())
}

fn ui(frame: &mut Frame, state: &mut AppState) {
    // let (log_size, progress_size) = match state.download_status {
    //     Some(_) => (7, 3),
    //     None => (10, 0),
    // };
    let (log_size, progress_size) = (7, 3);

    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(log_size),
            Constraint::Length(progress_size),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let items: Vec<ListItem> = state
        .items
        .iter()
        .map(|item| {
            let content = match item {
                FileListItem::Parent => "../".to_string(),
                FileListItem::Directory(name) => format!("{name}/"),
                FileListItem::File(name, _) => name.clone(),
            };
            ListItem::new(content)
        })
        .collect();

    let file_list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Files"))
        .highlight_style(
            Style::default()
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(file_list, main_layout[0], &mut state.selected_item);

    let log_messages: Vec<Line> = state
        .logs
        .iter()
        .rev()
        .take((log_size - 2) as usize)
        .rev()
        .map(|msg| Line::from(msg.clone()))
        .collect();
    let log_widget = Paragraph::new(log_messages)
        .block(Block::default().borders(Borders::ALL).title("Logs"))
        .wrap(Wrap { trim: false });
    frame.render_widget(log_widget, main_layout[1]);

    if let Some((name, progress)) = &state.download_status {
        let (label, ratio) = match progress {
            DownloadProgress::InProgress(downloaded, total) => (
                format!(
                    "Downloading '{}' {}/{}...",
                    name,
                    humansize::format_size(*downloaded, humansize::BINARY),
                    humansize::format_size(*total, humansize::BINARY)
                ),
                if *total > 0 {
                    *downloaded as f64 / *total as f64
                } else {
                    0.0
                },
            ),
            _ => (String::new(), 0.0),
        };

        let gauge = Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Download Progress"),
            )
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(ratio)
            .label(label);
        frame.render_widget(gauge, main_layout[2]);
    }

    let status_bar = Paragraph::new(Line::from(format!(
        "Path: {}",
        state.current_path.display()
    )))
    .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(status_bar, main_layout[3]);
}
