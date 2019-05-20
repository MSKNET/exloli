#[macro_use]
extern crate log;

use crate::{
    config::Config,
    exhentai::{BasicGalleryInfo, ExHentai},
    telegram::Bot,
};
use chrono::{prelude::*, Duration};
use failure::Error;
use rayon::prelude::*;
use reqwest::Client;
use std::{
    fs,
    io::{self, Read, Write},
    sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    },
    thread::sleep,
    time,
};
use telegraph_rs::{html_to_node, Telegraph, UploadResult};
use tempfile::NamedTempFile;

mod config;
mod exhentai;
mod telegram;
mod xpath;

/// 通过 URL 上传图片至 telegraph
pub fn upload_by_url(url: &str) -> Result<UploadResult, Error> {
    let client = Client::new();
    // 下载图片
    debug!("下载图片: {}", url);
    let mut file = NamedTempFile::new()?;
    let mut response = client.get(url).send()?;
    io::copy(&mut response, &mut file)?;

    // 上传图片
    debug!("上传图片: {:?}", file.path());
    let result = Telegraph::upload(&[file.path()])?.swap_remove(0);
    Ok(result)
}

fn run(config: &Config) -> Result<(), Error> {
    info!("登录中...");
    let bot = Bot::new(&config.telegram.token);
    let exhentai = ExHentai::new(&config.exhentai.username, &config.exhentai.password)?;
    let telegraph = telegraph_rs::Telegraph::new(&config.telegraph.author_name)
        .author_url(&config.telegraph.author_url)
        .access_token(&config.telegraph.access_token)
        .create()?;

    let mut page = -1;
    let galleries = std::iter::from_fn(|| {
        page += 1;
        exhentai.search(&config.exhentai.keyword, page).ok()
    });

    let last_time = if std::path::Path::new("./LAST_TIME").exists() {
        let mut s = String::new();
        fs::File::open("./LAST_TIME")?.read_to_string(&mut s)?;
        s.parse::<DateTime<Local>>()?
    } else {
        // 默认从两天前开始
        Local::now() - Duration::days(2)
    };
    debug!("截止时间: {:?}", last_time);

    let galleries = galleries
        .flatten()
        // FIXME: 由于时间只精确到分钟, 此处存在极小的忽略掉本子的可能性
        .take_while(|gallery| gallery.post_time > last_time)
        .collect::<Vec<BasicGalleryInfo>>();

    for gallery in galleries.into_iter().rev() {
        info!("画廊名称: {}", gallery.title);
        info!("画廊地址: {}", gallery.url);

        let gallery_info = gallery.get_full_info()?;

        // 多线程爬取图片并上传至 telegraph
        let i = Arc::new(AtomicU32::new(0));
        let img_urls = gallery_info
            .img_pages
            .par_iter()
            .map(|url| {
                let now = i.load(SeqCst);
                info!(
                    "第 {} / {} 张图片",
                    now + 1,
                    gallery_info.img_pages.len()
                );
                i.store(now + 1, SeqCst);
                loop {
                    let img_url = gallery
                        .get_image_url(url)
                        .and_then(|img_url| upload_by_url(&img_url))
                        .map(|result| result.src.to_owned());
                    match img_url {
                        Ok(v) => break Ok(v),
                        Err(e) => {
                            error!("获取图片地址失败: {}", e);
                            sleep(time::Duration::from_secs(10));
                        }
                    }
                }
            })
            .collect::<Result<Vec<String>, Error>>()?;

        let content = html_to_node(
            &img_urls
                .iter()
                .map(|s| format!(r#"<img src="{}">"#, s))
                .collect::<Vec<_>>()
                .join(""),
        );
        info!("发布文章");

        let article_url = loop {
            let result = telegraph.create_page(&gallery.title, &content, false);
            match result {
                Ok(v) => break v.url.to_owned(),
                Err(e) => {
                    error!("发布文章失败: {}", e);
                    sleep(time::Duration::from_secs(10));
                }
            }
        };
        info!("文章地址: {}", article_url);
        let tags = gallery_info
            .tags
            .iter()
            .map(|(k, v)| {
                format!(
                    "<code>{:>9}</code>: {}",
                    k,
                    v.iter()
                        // FIXME: tag 中可能含有 |
                        .map(|s| format!("#{}", s.replace(' ', "_")))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        bot.send_message(
            &config.telegram.channel_id,
            &format!(
                "{}\n<a href=\"{}\">{}</a>",
                tags, article_url, gallery.title
            ),
            &gallery.url,
        )?;

        fs::File::create("./LAST_TIME")?.write_all(gallery.post_time.to_rfc3339().as_bytes())?;
    }

    Ok(())
}

fn main() {
    let config = Config::new("config.toml").unwrap_or_else(|e| {
        eprintln!("配置文件解析失败:\n{}", e);
        std::process::exit(1);
    });

    // 设置相关环境变量
    if let Some(log_level) = config.log_level.as_ref() {
        std::env::set_var("RUST_LOG", format!("exloli={}", log_level));
    }
    if let Some(threads_num) = config.threads_num.as_ref() {
        std::env::set_var("RAYON_NUM_THREADS", threads_num);
    }

    env_logger::init();

    loop {
        match run(&config) {
            Ok(()) => {
                info!("任务完成!");
                return;
            }
            Err(e) => {
                error!("任务出错: {}", e);
                sleep(time::Duration::from_secs(60));
            }
        }
    }
}
