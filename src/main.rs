use clap::Parser;
use anyhow::{Result};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{header::USER_AGENT, Client, Url};
use subparse::{timetypes::{TimeSpan, TimePoint}, SrtFile, SubtitleFileInterface};
use std::{io::Write, path::PathBuf, fs, fs::File, cmp::min, process::{Command, Stdio}};
use log::{info};
use scraper::{Html, Selector, ElementRef};
use futures_util::StreamExt;

#[derive(Debug, Parser)]
struct Cli {
    url: String,
    path: PathBuf,
    #[clap(flatten)]
    verbose: clap_verbosity_flag::Verbosity,
}

enum VideoType {
    HD,
    SD
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .init();

    info!("starting up");

    let client = reqwest::Client::new();
    let res = client.get(&args.url)
    .header(USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/101.0.4951.64 Safari/537.36")
    .send().await?;

    info!("Response: {:?} {}", res.version(), res.status());
    // eprintln!("Headers: {:#?}\n", res.headers());

    let body = res.text().await?;

    let html = Html::parse_document(&body);

    let link = parse_video(&html, &VideoType::HD);
    let video_name = download_video(&client, &link, &args.path).await.unwrap();

    let mut times = vec![];
    let mut texts = vec![];
    let srt_name = video_name.split(".").nth(0).unwrap().to_owned() + ".srt";
    let mut srt_path = PathBuf::from(&args.path);
    srt_path.push(&srt_name);

    parse_transcript(&html, &mut times, &mut texts);
    generate_srt_file(&srt_path, &times, &texts);

    embed_subtitle(&video_name, &srt_name);

    info!("Done.");
    Ok(())
}

fn parse_transcript(html: &Html, times: &mut Vec<i64>, texts: &mut Vec<String>) {
    info!("parsing transcript");
    let p_selector = Selector::parse(r#"li.supplement.transcript p"#).unwrap();
    let sentence_selector = Selector::parse("span.sentence").unwrap();

    for p_element in html.select(&p_selector) {
        for element in p_element.select(&sentence_selector) {
            let span_node = element.first_child().unwrap();
            let span_element = ElementRef::wrap(span_node).unwrap();
            let time_str = span_element.value().attr("data-start").unwrap();
            let time_float: f64 = time_str.parse().expect(&format!("{} is not a digit", time_str));
            let time: i64 = (time_float * 1000.0) as i64;
            let text = span_element.inner_html().to_string();
            info!("{}:{}", time.to_string(), text);
            
            times.push(time);
            texts.push(text);
        }
    }
}

fn generate_srt_file(path: &PathBuf, times: &Vec<i64>, texts: &Vec<String>) {
    info!("generating subtitle");
    if path.exists() { return }
    let mut lines = vec![];
    for (i, text) in texts.iter().enumerate() {
        let start_time = *times.get(i).unwrap();
        let end_time: i64;
        if let Some(time) = times.get(i+1) {
            end_time = *time;
        } else {
            end_time = start_time + 3000;
        }

        let line = (
            TimeSpan::new(TimePoint::from_msecs(start_time), TimePoint::from_msecs(end_time)),
            String::from(text),
        );
        lines.push(line);
    }

    let file = SrtFile::create(lines).unwrap();
    let data = file.to_data().unwrap();
    fs::write(path, data).expect("Unable to write file");
}


fn parse_video(html: &Html, video_type: &VideoType) -> String {
    info!("parsing video");
    let video_selectgor = Selector::parse(r#"li.download ul li a"#).unwrap();
    let mut link = String::from("");
    for a_el in html.select(&video_selectgor) {
        let video_type_str = a_el.inner_html();
        match video_type {
            VideoType::HD => {
                if video_type_str.contains("HD") {
                    let href = a_el.value().attr("href").unwrap();
                    link = String::from(href);
                    info!("hd href: {}", link);
                    break;
                }
            }
            VideoType::SD => {
                if video_type_str.contains("SD") {
                    let href = a_el.value().attr("href").unwrap();
                    link = String::from(href);
                    info!("sd href: {}", link);
                    break;
                }
            }
        }
    }
    
    link
}

async fn download_video(client: &Client, url: &str, path: &PathBuf) -> Result<String, String> {
    info!("downloading video");
    let u = Url::parse(url).unwrap();
    let file_name = u.path().split("/").last().unwrap();
    let mut saved_path = PathBuf::from(path);
    saved_path.push(file_name);

    if saved_path.exists() {
        return Ok(String::from(file_name));
    }
    // Reqwest setup
    let res = client.get(url).send().await.or(Err(format!("Failed to GET from '{}'", &url)))?;

    let total_size = res.content_length().ok_or(format!("Failed to get content length from '{}'", &url))?;

    // Indicatif setup
    let pb = ProgressBar::new(total_size);
    pb.set_style(ProgressStyle::default_bar()
        .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
        .progress_chars("#>-")
    );
    pb.set_message(format!("Downloading {}", url));

    // download chunks
    let mut file = File::create(&saved_path).or(Err(format!("Failed to create file '{}'", saved_path.to_str().unwrap())))?;
    let mut downloaded: u64 = 0;
    let mut stream = res.bytes_stream();

    while let Some(item) = stream.next().await {
        let chunk = item.or(Err(format!("Error while downloading file")))?;
        file.write_all(&chunk)
            .or(Err(format!("Error while writing to file")))?;
        let new = min(downloaded + (chunk.len() as u64), total_size);
        downloaded = new;
        pb.set_position(new);
    }

    pb.finish_with_message(format!("Downloaded {} to {}", url, saved_path.to_str().unwrap()));
    return Ok(String::from(file_name));
}

fn embed_subtitle(video_name: &str, subtitle_name: &str) {
    info!("embeding subtitle");
    let child = Command::new("ffmpeg")
        // Overwrite file if it already exists
        .arg("-y")
        // Get the data from stdin
        .arg("-i")
        .arg(video_name)
        .arg("-i")
        .arg(subtitle_name)
        .arg("-map")
        .arg("0")
        .arg("-map")
        .arg("1")
        .arg("-c:v")
        .arg("copy")
        .arg("-c:a")
        .arg("copy")
        .arg("-c:s")
        .arg("mov_text")
        .arg("-metadata:s:s:0")
        .arg("language=eng")
        // Output file
        .arg("output.mp4")
        // stdin, stderr, and stdout are piped
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        // Run the child command
        .spawn()
        .unwrap();

    let output = child.wait_with_output().unwrap();
    info!("{}", String::from_utf8(output.stdout).unwrap());
    info!("{}", String::from_utf8(output.stderr).unwrap());
    info!("status: {}", output.status);
}