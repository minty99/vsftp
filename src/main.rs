use clap::Parser;
use color_eyre::eyre::{self, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use skim::prelude::*;
use ssh2::{FileStat, Session};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::TcpStream;
use std::path::PathBuf;

use tokio::task::JoinHandle;

/// An interactive SFTP client with fuzzy finding
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Remote host to connect to (e.g., user@host)
    remote: String,

    /// Port to connect to
    #[arg(short, long, default_value_t = 22)]
    port: u16,

    /// Initial remote path to browse
    #[arg(default_value = ".")]
    path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();

    let (user, host) = match args.remote.split_once('@') {
        Some((user, host)) => (user.to_string(), host.to_string()),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
            (user, args.remote)
        }
    };

    println!("Connecting to {}@{}:{}...", user, host, args.port);

    let tcp = TcpStream::connect(format!("{}:{}", host, args.port))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    let password = rpassword::prompt_password(format!("Password for {user}: "))?;
    sess.userauth_password(&user, &password)?;

    if !sess.authenticated() {
        eyre::bail!("Authentication failed. Please check your credentials.");
    }

    println!("\nAuthentication successful!");

    let sftp = sess.sftp()?;

    let mut current_path = PathBuf::from(args.path);

    loop {
        println!("Fetching file list from '{}'...", current_path.display());

        let files: HashMap<String, (PathBuf, FileStat)> = {
            sftp.readdir(&current_path)?
                .into_iter()
                .map(|(path, stat)| {
                    let name = path.file_name().unwrap().to_str().unwrap();
                    (name.to_string(), (path, stat))
                })
                .collect()
        };
        let file_list_str = generate_selections(&files)?;
        let selected_item = run_skim(file_list_str)?;

        // TODO: now, if file name contains '/', it will be considered as directory.
        // TODO: now, if file name contains parentheses or whitespace, it will not work.
        if selected_item.ends_with('/') {
            // Directory
            current_path.push(selected_item);
            continue;
        }
        // File
        let file_name = selected_item.split(' ').next().unwrap_or("");
        let (remote_path, file_size) = files
            .iter()
            .find(|(name, _)| *name == file_name)
            .map(|(_, (path, stat))| (path.clone(), stat.size.unwrap_or(0)))
            .unwrap();

        println!("\nStarting downloads...");
        let m = MultiProgress::new();
        let mut download_handles = Vec::new();

        let file_name = remote_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let pb = m.add(ProgressBar::new(file_size));
        pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}) {msg}")
                    .unwrap()
                    .progress_chars("##-"),
            );
        pb.set_message(file_name.clone());

        let sess_clone = sess.clone();
        let file_name_clone = file_name.clone();

        let handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
            let result = (|| {
                let sftp = sess_clone.sftp()?;
                let mut remote_file = sftp.open(&remote_path)?;
                let mut local_file = std::fs::File::create(&file_name)?;

                let mut reader = pb.wrap_read(&mut remote_file);
                std::io::copy(&mut reader, &mut local_file)?;
                Ok(())
            })();

            match &result {
                Ok(_) => {
                    pb.finish_with_message(format!("{file_name_clone} - Download complete!"));
                }
                Err(e) => {
                    pb.finish_with_message(format!("{file_name_clone} - Download failed: {e}"));
                }
            }

            result
        });
        download_handles.push(handle);
    }
}

fn generate_selections(files: &HashMap<String, (PathBuf, FileStat)>) -> Result<String> {
    let file_list_str = files
        .iter()
        .map(|(name, (_, stat))| {
            if stat.is_dir() {
                format!("{name}/")
            } else {
                format!(
                    "{} ({})",
                    name,
                    indicatif::HumanBytes(stat.size.unwrap_or(0))
                )
            }
        })
        .collect::<Vec<String>>()
        .join("\n");

    Ok(file_list_str)
}

fn run_skim(file_list_str: String) -> Result<String> {
    let options = SkimOptionsBuilder::default()
        .height("50%".to_string())
        .build()
        .unwrap();

    let item_reader = SkimItemReader::default();
    let items = item_reader.of_bufread(Cursor::new(file_list_str));

    let selected_item =
        Skim::run_with(&options, Some(items)).and_then(|out| out.selected_items.first().cloned());

    match selected_item {
        Some(item) => Ok(item.output().into()),
        None => Ok("".to_string()),
    }
}
