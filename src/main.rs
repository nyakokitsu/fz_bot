mod api;

use api::{FZClient, ServerStatus};
use teloxide::{
    payloads::{EditMessageTextSetters, SendMessageSetters, SendPhotoSetters}, prelude::*, types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ReplyParameters}
};
use tokio::time::{sleep, Duration};
use std::env;

fn get_main_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Старт", "choose_save"),
            InlineKeyboardButton::callback("Стоп", "stop_server"),
        ],
        vec![InlineKeyboardButton::callback("Сохранения", "manage_saves")],
    ])
}

fn get_back_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("◀", "main_menu")]])
}

#[tokio::main]
async fn main() {
    env_logger::init();
    log::info!("Starting Factorio Zone Telegram bot...");

    let token = env::var("TELEGRAM_TOKEN")
        .expect("TELEGRAM_TOKEN environment variable not set");
    let factorio_token = env::var("FACTORIO_TOKEN")
        .expect("FACTORIO_TOKEN environment variable not set");

    let bot = Bot::new(token);
    let client = FZClient::new(&factorio_token);

    let client_clone = client.clone();
    tokio::spawn(async move {
        if let Err(e) = client_clone.connect().await {
            log::error!("WS connection lost: {:?}", e);
        }
    });

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(message_handler))
        .branch(Update::filter_callback_query().endpoint(callback_handler));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![client])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn message_handler(bot: Bot, msg: Message, client: FZClient) -> ResponseResult<()> {
    let text = msg.text().unwrap_or_default();

    if text == "/start" {
        let state = client.state.read().await;
        let info = format!(
            "Статус сервера: <code>{}</code>\nIP: <code>{}</code>",
            state.server_status,
            state.server_address.as_deref().unwrap_or("N/A")
        );
        bot.send_message(msg.chat.id, info)
            .parse_mode(teloxide::types::ParseMode::Html)
            .reply_markup(get_main_menu())
            .await?;
        return Ok(());
    }

    let re = regex::Regex::new(r"^(https?://)?factoriobin\.com/post/(.+)$").unwrap();
    if let Some(caps) = re.captures(text) {
        let post_id = &caps[2];
        let http = reqwest::Client::new();
        let api_url = format!("https://factoriobin.com/post/{}/info.json", post_id);
        
        if let Ok(res) = http.get(&api_url).send().await {
            if let Ok(resp) = res.json::<serde_json::Value>().await {
                if let Some(img_url) = resp["node"]["renderImageUrl"].as_str() {
                    bot.send_photo(msg.chat.id, InputFile::url(img_url.parse().unwrap()))
                        .reply_parameters(ReplyParameters::new(msg.id))
                        .await?;
                }
            }
        }
    }

    Ok(())
}

async fn callback_handler(bot: Bot, q: CallbackQuery, client: FZClient) -> ResponseResult<()> {
    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };
    let message = match q.message {
        Some(m) => m,
        None => return Ok(()),
    };

    match data {
        "main_menu" => {
            let state = client.state.read().await;
            let text = format!(
                "Статус сервера: <code>{}</code>\nIP: <code>{}</code>",
                state.server_status,
                state.server_address.as_deref().unwrap_or("N/A")
            );
            bot.edit_message_text(message.chat().id, message.id(), text)
                .parse_mode(teloxide::types::ParseMode::Html)
                .reply_markup(get_main_menu())
                .await?;
        }
        "choose_save" => {
            let keyboard = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("Vanilla", "start_save:1")],
                vec![InlineKeyboardButton::callback("Space Age", "start_save:2")],
                vec![InlineKeyboardButton::callback("Назад", "main_menu")],
            ]);
            bot.edit_message_text(message.chat().id, message.id(), "Выберите сохранение для запуска:")
                .reply_markup(keyboard)
                .await?;
        }
        "stop_server" => {
            bot.edit_message_text(message.chat().id, message.id(), "Останавливаем...").await?;
            let _ = client.stop_instance().await;
            
            loop {
                let status = client.state.read().await.server_status;
                if status == ServerStatus::OFFLINE { break; }
                sleep(Duration::from_secs(5)).await;
            }
            bot.edit_message_text(message.chat().id, message.id(), "Сервер остановлен.")
                .reply_markup(get_back_menu())
                .await?;
        }
        "manage_saves" => {
            let keyboard = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("Vanilla", "download_save:1")],
                vec![InlineKeyboardButton::callback("Space Age", "download_save:2")],
                vec![InlineKeyboardButton::callback("Назад", "main_menu")],
            ]);
            bot.edit_message_text(message.chat().id, message.id(), "Выберите сохранение для скачивания:")
                .reply_markup(keyboard)
                .await?;
        }
        _ if data.starts_with("start_save:") => {
            let slot = data.split(':').nth(1).unwrap_or("1").to_string();
            bot.answer_callback_query(q.id).await?;

            let poll_msg = bot.send_poll(
                message.chat().id,
                "Запускаем сервер?",
                vec!["Да".to_string(), "Нет".to_string()],
            )
            .is_anonymous(false)
            .open_period(62)
            .await?;

            // Poll run timeout window (60s)
            sleep(Duration::from_secs(60)).await;

            let poll_results = bot.stop_poll(message.chat().id, poll_msg.id).await?;
            let _ = bot.delete_message(message.chat().id, poll_msg.id).await;

            let yes_votes = poll_results.options[0].voter_count;
            let total_votes: serde_json::Number = poll_results.total_voter_count.into();
            let total_votes = total_votes.as_u64().unwrap_or(0);

            if total_votes == 0 {
                bot.send_message(message.chat().id, "<i>но никто не пришел...</i>\nГолосование окончено.")
                    .parse_mode(teloxide::types::ParseMode::Html).await?;
                return Ok(());
            }
            if total_votes < 3 && total_votes > 1 {
                bot.send_message(message.chat().id, "Проголосовало меньше 3 человек.\nГолосование окончено.").await?;
                return Ok(());
            }

            let percentage = (yes_votes as f64 / total_votes as f64) * 100.0;
            let need_run = percentage > 60.0;

            let result_text = format!(
                "Голосование окончено.\n\n<blockquote>Всего голосов: {}\nГолосов за <code>Да</code>: {}</blockquote>\n\nСервер{} будет запущен.",
                total_votes, yes_votes, if !need_run { " не" } else { "" }
            );

            let _run_msg = bot.send_message(message.chat().id, result_text)
                .parse_mode(teloxide::types::ParseMode::Html).await?;

            if need_run {
                let mapped_slot = if slot == "2" { "slot2" } else { "slot1" };
                let _ = client.start_instance("eu-north-1", "2.0.55", mapped_slot).await;
                
                let running_status_msg = bot.send_message(message.chat().id, "Сервер запускается...").await?;
                
                loop {
                    let status = client.state.read().await.server_status;
                    if status == ServerStatus::RUNNING { break; }
                    sleep(Duration::from_secs(5)).await;
                }

                let addr = client.state.read().await.server_address.clone().unwrap_or_default();
                bot.edit_message_text(message.chat().id, running_status_msg.id, format!("<code>{}</code>", addr))
                    .parse_mode(teloxide::types::ParseMode::Html)
                    .reply_markup(get_back_menu())
                    .await?;
            }
        }
        _ if data.starts_with("download_save:") => {
            let slot = data.split(':').nth(1).unwrap_or("1");
            let mapped_slot = if slot == "2" { "slot2" } else { "slot1" };

            bot.edit_message_text(message.chat().id, message.id(), format!("Загрузка сейва {}...", slot)).await?;

            match client.download_save_slot(mapped_slot).await {
                Ok(bytes_data) => {
                    let input_file = InputFile::memory(bytes_data).file_name(format!("{}.zip", slot));
                    bot.send_document(message.chat().id, input_file)
                        .caption(format!("Сейв {}", slot))
                        .await?;
                }
                Err(e) => {
                    bot.edit_message_text(message.chat().id, message.id(), format!("Ошибка загрузки: {}", e)).await?;
                }
            }
        }
        _ => {}
    }

    Ok(())
}