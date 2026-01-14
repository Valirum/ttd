use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueHint};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use xdg::BaseDirectories;
use chrono::{DateTime, Utc, TimeDelta, Weekday, Datelike, NaiveDate, Duration};
use humantime::parse_duration;
use strsim::jaro_winkler;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
use std::io::Write;

#[derive(Deserialize)]
struct Config {
    app: AppConfig,
}

#[derive(Deserialize)]
struct AppConfig {
    timezone_offset_hours: i64,
    can_override: bool,
    exact_match_threshold: Option<f64>,
    strict_comparison: Option<bool>,
}

#[derive(Parser)]
#[command(
name = "ttd",
version = "1.2",
about = "Простой текстовый менеджер задач\n\
\n\
Управляет задачами в различных сессиях, поддерживает строгий и нестрогий поиск по имени, поиск по индексу, сортировку по времени и
цветовое форматирование вывода.\n\
\n\
Примеры использования:\n\
ttd a 'поставить чайник' in 2h\n\
ttd d 0\n\
ttd s work\n\
ttd l"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(visible_alias = "sessions")]
    Ss,
    #[command(visible_alias = "session")]
    S { session: Option<String> },
    #[command(visible_alias = "add")]
    A { #[arg(num_args(1..), value_hint = ValueHint::CommandString)] parts: Vec<String> },
    #[command(visible_alias = "remove")]
    R { #[arg(num_args(1..), value_hint = ValueHint::CommandString)] parts: Vec<String> },
    #[command(visible_alias = "remove-session")]
    Rs { #[arg(num_args(1..), value_hint = ValueHint::CommandString)] parts: Vec<String> },
    #[command(visible_alias = "done")]
    D { #[arg(num_args(1..), value_hint = ValueHint::CommandString)] parts: Vec<String> },
    #[command(visible_alias = "undone")]
    Ud { #[arg(num_args(1..), value_hint = ValueHint::CommandString)] parts: Vec<String> },
    #[command(visible_alias = "time")]
    T { #[arg(num_args(1..), value_hint = ValueHint::CommandString)] parts: Vec<String> },
    #[command(visible_alias = "list")]
    L,
    #[command(visible_alias = "list-all")]
    Ll,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
struct Task {
    description: String,
    time: Option<DateTime<Utc>>,
    done: bool,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct Data {
    current_session: Option<String>,
    sessions: HashMap<String, Vec<Task>>,
}

fn parse_relative_time(input: &str) -> Result<DateTime<Utc>> {
    if let Ok(dur) = parse_duration(input) {
        return Ok(Utc::now() + TimeDelta::from_std(dur)?);
    }

    let mut dt = Utc::now();
    let mut i = 0;
    while i < input.len() {
        let mut num = 0u64;
        while i < input.len() && input.as_bytes()[i].is_ascii_digit() {
            num = num * 10 + (input.as_bytes()[i] - b'0') as u64;
            i += 1;
        }
        if i >= input.len() { break; }
        let unit = input.as_bytes()[i] as char;
        i += 1;
        match unit {
            'y' => dt += TimeDelta::days((num * 365) as i64),
            'M' => dt += TimeDelta::days((num * 30) as i64),
            'd' => dt += TimeDelta::days(num as i64),
            'h' => dt += TimeDelta::hours(num as i64),
            'm' => dt += TimeDelta::minutes(num as i64),
            's' => dt += TimeDelta::seconds(num as i64),
            _ => {}
        }
    }
    Ok(dt)
}

fn parse_absolute_time(input: &str, offset_hours: i64) -> Result<DateTime<Utc>> {
    let now_utc = Utc::now();
    let offset = TimeDelta::hours(offset_hours);
    let now_local = now_utc + offset;
    let now_naive = now_local.naive_utc();

    let mut year: Option<i32> = None;
    let mut month: Option<u32> = None;
    let mut day: Option<u32> = None;
    let mut hour: Option<u32> = None;
    let mut minute: Option<u32> = None;
    let mut second: Option<u32> = None;
    let mut weekday_target: Option<u32> = None;

    let mut i = 0;
    while i < input.len() {
        let mut num = 0u64;
        while i < input.len() && input.as_bytes()[i].is_ascii_digit() {
            num = num * 10 + (input.as_bytes()[i] - b'0') as u64;
            i += 1;
        }
        if i >= input.len() { break; }

        let unit = input.as_bytes()[i] as char;
        i += 1;

        match unit {
            'y' => {
                if num > 9999 {
                    anyhow::bail!("Year must be between 1 and 9999");
                }
                year = Some(num as i32);
            }
            'M' => {
                if num < 1 || num > 12 {
                    anyhow::bail!("Month must be between 1 and 12");
                }
                month = Some(num as u32);
            }
            'd' => {
                if num < 1 || num > 31 {
                    anyhow::bail!("Day must be between 1 and 31");
                }
                day = Some(num as u32);
            }
            'w' => {
                if num < 1 || num > 7 {
                    anyhow::bail!("Weekday must be 1-7 (1=Monday, 7=Sunday)");
                }
                weekday_target = Some(num as u32);
            }
            'h' => {
                if num > 23 {
                    anyhow::bail!("Hour must be between 0 and 23");
                }
                hour = Some(num as u32);
            }
            'm' => {
                if num > 59 {
                    anyhow::bail!("Minute must be between 0 and 59");
                }
                minute = Some(num as u32);
            }
            's' => {
                if num > 59 {
                    anyhow::bail!("Second must be between 0 and 59");
                }
                second = Some(num as u32);
            }
            _ => {}
        }
    }

    if let Some(target_weekday_num) = weekday_target {
        if day.is_none() {
            let target_weekday = match target_weekday_num {
                1 => Weekday::Mon,
                2 => Weekday::Tue,
                3 => Weekday::Wed,
                4 => Weekday::Thu,
                5 => Weekday::Fri,
                6 => Weekday::Sat,
                7 => Weekday::Sun,
                _ => unreachable!(),
            };

            let current_weekday = now_naive.weekday();
            let mut days_ahead = (target_weekday.num_days_from_monday() as i64) -
            (current_weekday.num_days_from_monday() as i64);

            if days_ahead <= 0 {
                days_ahead += 7;
            }

            let target_date = now_naive.date() + Duration::days(days_ahead);
            day = Some(target_date.day());
            month = Some(target_date.month());
            year = Some(target_date.year());
        }
    }

    let final_year = year.unwrap_or(now_naive.year());
    let final_month = month.unwrap_or(now_naive.month());
    let final_day = day.unwrap_or(now_naive.day());
    let final_hour = hour.unwrap_or(0);
    let final_minute = minute.unwrap_or(0);
    let final_second = second.unwrap_or(0);

    let naive_local = NaiveDate::from_ymd_opt(final_year, final_month, final_day)
    .and_then(|date| date.and_hms_opt(final_hour, final_minute, final_second))
    .ok_or_else(|| anyhow::anyhow!(
        "Invalid date/time: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        final_year, final_month, final_day, final_hour, final_minute, final_second
    ))?;

    let naive_utc = naive_local - Duration::hours(offset_hours);
    let utc_dt = DateTime::from_naive_utc_and_offset(naive_utc, Utc);
    Ok(utc_dt)
}

fn get_time_color(time: &Option<DateTime<Utc>>) -> Color {
    match time {
        Some(t) if *t < Utc::now() => Color::Red,
        Some(_) => Color::Yellow,
        None => Color::Blue,
    }
}

fn get_status_color(done: bool) -> Color {
    if done { Color::Green } else { Color::Yellow }
}

fn print_formatted_task(i: usize, task: &Task, max_desc_len: usize, offset_hours: i64) -> Result<()> {
    let mut stdout = StandardStream::stdout(ColorChoice::Always);
    write!(stdout, "{:<2} ", i)?;

    let status_color = get_status_color(task.done);
    let status_text = if task.done { "[DONE]" } else { "[TODO]" };
    let mut status_spec = ColorSpec::new();
    status_spec.set_fg(Some(status_color));
    stdout.set_color(&status_spec)?;
    write!(stdout, "{:<8}", status_text)?;
    stdout.reset()?;

    let desc_text = if task.done {
        format!("\x1b[9m{:<width$}\x1b[0m", task.description, width = max_desc_len)
    } else {
        format!("{:<width$}", task.description, width = max_desc_len)
    };

    let time_str = format_time(&task.time, offset_hours);
    let time_color = get_time_color(&task.time);
    let mut time_spec = ColorSpec::new();
    time_spec.set_fg(Some(time_color));

    write!(stdout, "{} | ", desc_text)?;
    stdout.set_color(&time_spec)?;
    writeln!(stdout, "{}", time_str)?;
    stdout.reset()?;

    Ok(())
}

fn get_max_description_length(tasks: &[Task]) -> usize {
    tasks.iter()
    .map(|t| t.description.len())
    .max()
    .unwrap_or(0)
    .max(12)
}

fn get_data_path() -> Result<PathBuf> {
    let xdg = BaseDirectories::with_prefix("ttd")?;
    let path = xdg.place_config_file("tasks.json")?;
    Ok(path)
}

fn get_config_path() -> Result<PathBuf> {
    let xdg = BaseDirectories::with_prefix("ttd")?;
    let path = xdg.place_config_file("config.toml")?;
    Ok(path)
}

fn load_config() -> Result<(i64, bool, f64, bool)> {
    let path = get_config_path()?;
    let (default_offset, default_override, default_threshold, default_strict) = (3, true, 0.85, false);

    if path.exists() {
        let toml_str = fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&toml_str)?;
        let threshold = config.app.exact_match_threshold.unwrap_or(default_threshold);
        let strict = config.app.strict_comparison.unwrap_or(default_strict);
        Ok((
            config.app.timezone_offset_hours,
            config.app.can_override,
            threshold,
            strict
        ))
    } else {
        Ok((default_offset, default_override, default_threshold, default_strict))
    }
}

fn load_data() -> Result<Data> {
    let path = get_data_path()?;
    let mut data = if path.exists() {
        let json = fs::read_to_string(&path)?;
        if json.trim().is_empty() {
            Data::default()
        } else {
            match serde_json::from_str(&json) {
                Ok(data) => data,
                Err(_) => Data::default(),
            }
        }
    } else {
        Data::default()
    };

    for tasks in data.sessions.values_mut() {
        sort_tasks(tasks);
    }

    Ok(data)
}

fn save_data(data: &Data) -> Result<()> {
    let path = get_data_path()?;
    let json = serde_json::to_string_pretty(data)?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, json)?;
    Ok(())
}

fn find_by_index(tasks: &[Task], index: usize) -> Option<usize> {
    if index < tasks.len() { Some(index) } else { None }
}

fn find_by_name(tasks: &[Task], query: &str, threshold: f64, strict: bool) -> (Option<usize>, Option<(String, f64)>) {
    let query_lower = query.to_lowercase();

    if strict {
        let exact_match = tasks.iter().enumerate()
        .find(|(_, t)| t.description.to_lowercase() == query_lower)
        .map(|(i, _)| i);

        if let Some(idx) = exact_match {
            return (Some(idx), None);
        }

        let mut candidates: Vec<(usize, String, f64)> = tasks.iter().enumerate()
        .map(|(i, t)| {
            let score = jaro_winkler(&t.description.to_lowercase(), &query_lower);
            (i, t.description.clone(), score)
        })
        .filter(|(_, _, score)| *score > threshold)
        .collect();

        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        if !candidates.is_empty() {
            let (_idx, desc, score) = (candidates[0].0, candidates[0].1.clone(), candidates[0].2);
            return (None, Some((desc, score)));
        }

        (None, None)
    } else {
        let mut best_match = None;
        let mut best_score = 0.0;
        let mut best_index = None;

        for (i, task) in tasks.iter().enumerate() {
            let score = jaro_winkler(&task.description.to_lowercase(), &query_lower);
            if score > best_score && score >= threshold {
                best_score = score;
                best_index = Some(i);
                best_match = Some(task.description.clone());
            }
        }

        if let Some(idx) = best_index {
            return (Some(idx), Some((best_match.unwrap(), best_score)));
        }

        (None, None)
    }
}

fn find_task(tasks: &[Task], query: &str, threshold: f64, strict: bool) -> (Option<usize>, Option<(String, f64)>, bool) {
    if query.chars().all(|c| c.is_ascii_digit()) ||
        (query.starts_with('-') && query[1..].chars().all(|c| c.is_ascii_digit())) {
            match query.parse::<usize>() {
                Ok(idx) => (find_by_index(tasks, idx), None, true),
                Err(_) => (None, None, true)
            }
        } else {
            let (result, match_info) = find_by_name(tasks, query, threshold, strict);
            (result, match_info, false)
        }
}

fn format_time(dt: &Option<DateTime<Utc>>, offset_hours: i64) -> String {
    dt.map_or("end of times".to_string(), |t| {
        let offset = TimeDelta::hours(offset_hours);
        let local = t + offset;
        local.format("%Y-%m-%d %H:%M").to_string()
    })
}

fn sort_tasks(tasks: &mut Vec<Task>) {
    tasks.sort_by(|a, b| {
        match (&a.time, &b.time) {
            (Some(t1), Some(t2)) => t1.cmp(t2),
                  (Some(_), None) => std::cmp::Ordering::Less,
                  (None, Some(_)) => std::cmp::Ordering::Greater,
                  (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut data = load_data()?;
    let (offset_hours, can_override, match_threshold, strict_comparison) = load_config()?;

    // Исправлено: клонируем имя сессии, чтобы избежать заимствования
    let current_session_name = data.current_session.clone().unwrap_or_else(|| "default".to_string());

    match cli.command {
        Commands::Ss => {
            let sessions: Vec<_> = data.sessions.keys().cloned().collect();
            println!("Available sessions: {:?}", sessions);
            if let Some(curr) = &data.current_session {
                println!("Current session: {}", curr);
            } else {
                println!("No current session (using 'default')");
            }
        }
        Commands::S { session } => {
            if let Some(session_name) = session {
                data.sessions.entry(session_name.clone()).or_insert_with(Vec::new);
                data.current_session = Some(session_name.clone());
                println!("Switched to session '{}'", session_name);
            } else {
                if let Some(current_sess) = &data.current_session {
                    println!("Current session: '{}'", current_sess);
                } else {
                    println!("No current session (using 'default')");
                }
            }
        },
        Commands::A { parts } => {
            if parts.is_empty() {
                println!("Usage: a <task> [in|at] <time>");
                return Ok(());
            }
            let task_desc = parts[0].clone();

            if task_desc.chars().all(|c| c.is_ascii_digit()) {
                println!("Task name '{}' looks like index. Use letters!", task_desc);
                println!("Example: a 'buy milk'");
                return Ok(());
            }

            let time = if parts.len() > 2 {
                let prefix = &parts[1];
                let time_str = &parts[2];

                if prefix == "in" {
                    Some(parse_relative_time(time_str)?)
                } else if prefix == "at" {
                    Some(parse_absolute_time(time_str, offset_hours)?)
                } else {
                    println!("Unknown time prefix '{}'. Use 'in' for relative time or 'at' for absolute time.", prefix);
                    return Ok(());
                }
            } else {
                None
            };

            let sess = data.sessions.entry(current_session_name.clone()).or_insert_with(Vec::new);
            let (exact_match_idx, match_info, _) = find_task(sess, &task_desc, match_threshold, true);
            let exact_description_match = sess.iter().position(|t| t.description == task_desc);

            if let Some(idx) = exact_description_match {
                if can_override {
                    sess[idx].time = time;
                    sess[idx].done = false;
                    println!("Overrode existing task '{}'", task_desc);
                } else {
                    println!("Task '{}' already exists", task_desc);
                    println!("Set can_override=true in ~/.config/ttd/config.toml to override");
                    return Ok(());
                }
            } else if let Some(idx) = exact_match_idx {
                let (matched_desc, score) = match_info.unwrap();
                if strict_comparison {
                    println!("No exact match found for '{}'", task_desc);
                    println!("Possible match: \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                    println!("To enable fuzzy matching, set strict_comparison=false in config.toml");
                    return Ok(());
                } else {
                    println!("\"{}\" accepted as \"{}\" (confidence: {:.1}%)", task_desc, matched_desc, score * 100.0);
                    println!("To enable strict matching, set strict_comparison=true in config.toml");
                }

                if can_override {
                    println!("Overriding due to can_override=true");
                    sess[idx].time = time;
                    sess[idx].done = false;
                } else {
                    println!("Set can_override=true to override or use different name");
                    return Ok(());
                }
            } else {
                let task = Task {
                    description: task_desc.clone(),
                    time,
                    done: false,
                };
                sess.push(task);
                println!("Added new task '{}'", task_desc);
            }
            sort_tasks(sess);
        },
        Commands::R { ref parts } => {
            handle_remove(parts, &mut data, &current_session_name, match_threshold, strict_comparison)?;
        },
        Commands::Rs { parts } => {
            let session = &parts[0];
            if session.is_empty() {
                println!("Session name cannot be empty");
                return Ok(());
            }

            if session == "default" {
                println!("Cannot remove default session");
                return Ok(());
            }

            if !data.sessions.contains_key(session) {
                println!("Session '{}' not found", session);
                return Ok(());
            }

            let tasks = data.sessions.get(session).unwrap();
            let uncompleted_count = tasks.iter().filter(|t| !t.done).count();
            let total_count = tasks.len();

            if total_count > 0 {
                println!("Session '{}' contains {} tasks ({} uncompleted)",
                         session, total_count, uncompleted_count);

                if uncompleted_count > 0 {
                    println!("Are you sure you want to delete this session with uncompleted tasks? [y/N]");

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let input = input.trim().to_lowercase();

                    if input != "y" && input != "yes" {
                        println!("Session deletion cancelled");
                        return Ok(());
                    }
                } else {
                    println!("Session contains only completed tasks. Delete anyway? [y/N]");

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let input = input.trim().to_lowercase();

                    if input != "y" && input != "yes" {
                        println!("Session deletion cancelled");
                        return Ok(());
                    }
                }
            }

            // Удаляем сессию
            data.sessions.remove(session);

            // Если удаляемая сессия была текущей - переключаемся на default
            if Some(session) == data.current_session.as_ref() {
                data.current_session = Some("default".to_string());
                println!("Switched to default session");
            }

            println!("Session '{}' deleted successfully", session);
        },
        Commands::D { ref parts } => {
            handle_done(parts, &mut data, &current_session_name, match_threshold, strict_comparison, true)?;
        },
        Commands::Ud { ref parts } => {
            handle_done(parts, &mut data, &current_session_name, match_threshold, strict_comparison, false)?;
        },
        Commands::T { ref parts } => {
            if parts.len() < 1 {
                println!("Usage: t <index|task_name> [in|at] <time>");
                return Ok(());
            }
            let query = &parts[0];
            let sess = data.sessions.get_mut(&current_session_name).context("No session")?;

            let (target_idx, match_info, is_index_search) = find_task(sess, query, match_threshold, strict_comparison);

            if let Some(idx) = target_idx {
                let time = if parts.len() > 2 {
                    let prefix = &parts[1];
                    let time_str = &parts[2];

                    if prefix == "in" {
                        Some(parse_relative_time(time_str)?)
                    } else if prefix == "at" {
                        Some(parse_absolute_time(time_str, offset_hours)?)
                    } else {
                        println!("Unknown time prefix '{}'. Use 'in' for relative time or 'at' for absolute time.", prefix);
                        return Ok(());
                    }
                } else {
                    None
                };
                let old_time = format_time(&sess[idx].time, offset_hours);
                sess[idx].time = time;
                let new_time = format_time(&sess[idx].time, offset_hours);
                println!("Changed time for '{}': {} -> {}", sess[idx].description, old_time, new_time);
            } else {
                if !is_index_search {
                    if let Some((matched_desc, score)) = match_info {
                        if strict_comparison {
                            println!("No exact match found for '{}'", query);
                            println!("Possible match: \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                            println!("To enable fuzzy matching, set strict_comparison=false in config.toml");
                        } else {
                            println!("No task found matching \"{}\" with confidence > {:.0}%", query, match_threshold * 100.0);
                            println!("Closest match was \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                            println!("To enable strict matching, set strict_comparison=true in config.toml");
                        }
                    } else {
                        println!("Task '{}' not found", query);
                    }
                } else {
                    println!("Index {} not found", query);
                }
            }
            sort_tasks(sess);
        }
        Commands::L => {
            println!("Current session: '{}'", current_session_name);

            let sess_slice = data.sessions.get(&current_session_name).map_or(&[][..], |v| v.as_slice());
            if sess_slice.is_empty() {
                println!("No tasks in session '{}'", current_session_name);
                return Ok(());
            }

            let completed = sess_slice.iter().filter(|t| t.done).count();
            println!("Completed: {}/{}", completed, sess_slice.len());

            let max_desc_len = get_max_description_length(sess_slice);
            println!("{:<2} {:<7} {:<width$} | {}",
                     "№", "STATUS", "DESCRIPTION", "TIME",
                     width = max_desc_len);
            println!("{:-<2} {:-<7} {:-<width$} | {:-<16}",
                     "", "", "", "",
                     width = max_desc_len);

            for (i, t) in sess_slice.iter().enumerate() {
                print_formatted_task(i, t, max_desc_len, offset_hours)?;
            }
        },
        Commands::Ll => {
            if data.sessions.is_empty() {
                println!("No sessions available");
                return Ok(());
            }

            let max_desc_len_all = data.sessions.values()
            .flat_map(|tasks| tasks.iter().map(|t| t.description.len()))
            .max()
            .unwrap_or(0)
            .max(5);

            for (session_name, tasks) in &data.sessions {
                let completed = tasks.iter().filter(|t| t.done).count();
                println!("\n\n---> Session: '{}' {} ({}/{})\n",
                         session_name,
                         if Some(session_name) == data.current_session.as_ref() { "[CURRENT]" } else { "" },
                             completed,
                         tasks.len());

                if tasks.is_empty() {
                    println!("  (empty)");
                    continue;
                }

                println!("  {:<2} {:<7} {:<width$} | {}",
                         "№", "STATUS", "DESCRIPTION", "TIME",
                         width = max_desc_len_all);
                println!("  {:-<2} {:-<7} {:-<width$} | {:-<16}",
                         "", "", "", "",
                         width = max_desc_len_all);

                for (i, t) in tasks.iter().enumerate() {
                    let mut stdout = StandardStream::stdout(ColorChoice::Always);
                    write!(stdout, "  ")?;
                    stdout.reset()?;
                    print_formatted_task(i, t, max_desc_len_all, offset_hours)?;
                }
            }
        },
    }

    save_data(&data)?;
    Ok(())
}

fn handle_remove(parts: &[String], data: &mut Data, current: &str, threshold: f64, strict: bool) -> Result<()> {
    if parts.is_empty() {
        println!("Usage: r <index|task_name>");
        return Ok(());
    }
    let arg = &parts[0];
    let sess = data.sessions.get_mut(current).context("No session")?;

    let (target_idx, match_info, is_index_search) = find_task(sess, arg, threshold, strict);

    if let Some(idx) = target_idx {
        let desc = sess[idx].description.clone();
        sess.remove(idx);
        println!("Removed task #{} '{}'", idx, desc);
    } else {
        if !is_index_search {
            if let Some((matched_desc, score)) = match_info {
                if strict {
                    println!("No exact match found for '{}'", arg);
                    println!("Possible match: \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                    println!("To enable fuzzy matching, set strict_comparison=false in config.toml");
                } else {
                    println!("No task found matching \"{}\" with confidence > {:.0}%", arg, threshold * 100.0);
                    println!("Closest match was \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                    println!("To enable strict matching, set strict_comparison=true in config.toml");
                }
            } else {
                println!("Task '{}' not found", arg);
            }
        } else {
            println!("Index {} not found", arg);
        }
    }
    Ok(())
}

fn handle_done(parts: &[String], data: &mut Data, current: &str, threshold: f64, strict: bool, mark_done: bool) -> Result<()> {
    if parts.is_empty() {
        println!("Usage: {} <index|task_name>", if mark_done { "d" } else { "ud" });
        return Ok(());
    }
    let arg = &parts[0];
    let sess = data.sessions.get_mut(current).context("No session")?;

    let (target_idx, match_info, is_index_search) = find_task(sess, arg, threshold, strict);

    if let Some(idx) = target_idx {
        let desc = sess[idx].description.clone();
        if sess[idx].done != mark_done {
            sess[idx].done = mark_done;
            println!("Marked #{} '{}' as {}", idx, desc, if mark_done { "done" } else { "NOT done" });
        } else {
            println!("Task #{} '{}' is already {}", idx, desc, if mark_done { "done" } else { "NOT done" });
        }
    } else {
        if !is_index_search {
            if let Some((matched_desc, score)) = match_info {
                if strict {
                    println!("No exact match found for '{}'", arg);
                    println!("Possible match: \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                    println!("To enable fuzzy matching, set strict_comparison=false in config.toml");
                } else {
                    println!("No task found matching \"{}\" with confidence > {:.0}%", arg, threshold * 100.0);
                    println!("Closest match was \"{}\" (confidence: {:.1}%)", matched_desc, score * 100.0);
                    println!("To enable strict matching, set strict_comparison=true in config.toml");
                }
            } else {
                println!("Task '{}' not found", arg);
            }
        } else {
            println!("Index {} not found", arg);
        }
    }
    Ok(())
}
