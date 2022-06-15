use std::{env, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use futures_lite::stream::StreamExt;
use lapin::{options::BasicPublishOptions, BasicProperties};
use log::info;
use serde::{Deserialize, Serialize};
use teloxide::{
    dispatching::{
        dialogue::{self, serializer::Json, ErasedStorage, GetChatId, SqliteStorage, Storage},
        UpdateHandler,
    },
    net::Download,
    prelude::*,
    types::{File as TgFile, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode},
};
use tokio::fs::File;

type MyDialogue = Dialogue<State, ErasedStorage<State>>;
type MyStorage = std::sync::Arc<ErasedStorage<State>>;
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Serialize, Deserialize)]
pub enum State {
    Start,
    ReceiveFullName,
    ReceiveAge {
        full_name: String,
    },
    ReceiveLocation {
        full_name: String,
        age: u8,
    },
    ReceiveFromFiletype,
    ReceiveToFiletype {
        from_filetype: String,
    },
    ReceiveInputFile {
        from_filetype: String,
        to_filetype: String,
    },
}

impl Default for State {
    fn default() -> Self {
        Self::Start
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    // Connect to queue
    let amqp_addr = env::var("AMQP_ADDR").unwrap_or_else(|_| "amqp://127.0.0.1:5672".into());
    let amqp_conn = lapin::Connection::connect(
        &amqp_addr,
        lapin::ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio),
    )
    .await?;
    let amqp_conn = Arc::new(amqp_conn);

    info!("Connected to AMQP");

    // Setup bot
    info!("Starting dialogue bot ...");

    let bot = Bot::from_env();

    let storage: MyStorage = SqliteStorage::open(
        path_for_persistent_state()
            .join("dialogue.sqlite3")
            .to_str()
            .context("Failed to convert state path to str")?,
        Json,
    )
    .await
    .context("Failed to open SqliteStorage")?
    .erase();

    // Start the returning queue listener
    let returning_queue_task = tokio::spawn(listen_returning_queue(bot.clone(), amqp_conn.clone()));

    // Start the bot
    Dispatcher::builder(bot, bot_scheme())
        .dependencies(dptree::deps![storage, amqp_conn.clone()])
        .build()
        .setup_ctrlc_handler()
        .dispatch()
        .await;

    // Gracefully shutdown returning queue task
    amqp_conn.close(0, "").await?;
    returning_queue_task.await??;

    Ok(())
}

fn bot_scheme() -> UpdateHandler<Box<dyn std::error::Error + Send + Sync>> {
    dialogue::enter::<Update, ErasedStorage<State>, State, _>()
        .branch(
            Update::filter_message()
                .branch(dptree::case![State::Start].endpoint(start))
                .branch(
                    dptree::case![State::ReceiveInputFile {
                        from_filetype,
                        to_filetype
                    }]
                    .endpoint(receive_input_file),
                ),
        )
        .branch(
            Update::filter_callback_query()
                .branch(dptree::case![State::ReceiveFromFiletype].endpoint(receive_from_filetype))
                .branch(
                    dptree::case![State::ReceiveToFiletype { from_filetype }]
                        .endpoint(receive_to_filetype),
                ),
        )
}

/// Listen on the returning queue and return the results to bot users
async fn listen_returning_queue(bot: Bot, amqp_conn: Arc<lapin::Connection>) -> Result<()> {
    let channel = amqp_conn.create_channel().await?;
    let queue = channel
        .queue_declare("pandoc-outputs", Default::default(), Default::default())
        .await?;
    info!("Declared queue {queue:?}");
    let mut consumer = channel
        .basic_consume("pandoc-outputs", "", Default::default(), Default::default())
        .await?;
    while let Some(delivery) = consumer.next().await {
        let delivery = delivery?;
        let res: ConvertResponse = bson::from_slice(&delivery.data)?;

        delivery.ack(Default::default()).await?;

        match res {
            ConvertResponse::Success {
                chat_id,
                file,
                to_filetype,
            } => {
                info!("Received successful conversion");

                let text = format!("Converted succesffully to <b>{to_filetype}</b>!");

                let output_filename = format!("output.{}", filetype_to_extension(&to_filetype));
                let document = InputFile::memory(file).file_name(output_filename);

                bot.send_document(ChatId(chat_id), document)
                    .caption(text)
                    .parse_mode(ParseMode::Html)
                    .send()
                    .await?;
            }
            ConvertResponse::Failure { chat_id, error_msg } => {
                info!("Received failed conversion");

                bot.send_message(
                    ChatId(chat_id),
                    format!(
                        "Failed to perform the conversion:\n<pre>{}</pre>",
                        error_msg
                    ),
                )
                .parse_mode(ParseMode::Html)
                .send()
                .await?;
            }
        }

        info!("Got convert response from queue");
    }
    Ok(())
}

/* Bot handlers */

async fn start(bot: Bot, msg: Message, dialogue: MyDialogue) -> HandlerResult {
    let keyboard = make_from_keyboard();
    bot.send_message(
        msg.chat.id,
        "Let's start! Tell me the type of the original document.",
    )
    .reply_markup(keyboard)
    .send()
    .await?;

    dialogue.update(State::ReceiveFromFiletype).await?;
    Ok(())
}

async fn receive_from_filetype(bot: Bot, q: CallbackQuery, dialogue: MyDialogue) -> HandlerResult {
    bot.answer_callback_query(q.id.clone()).send().await?;
    let chat_id = q.chat_id().context("No chat id found")?;

    let make_fail_msg = || {
        let keyboard = make_from_keyboard();
        bot.send_message(chat_id, "Tell me the type of the original document.")
            .reply_markup(keyboard)
    };

    let make_success_msg = |from_filetype| {
        let keyboard = make_to_keyboard();

        let text = format!(
            "The type of the original document is set to <b>{}</b>. \
             What format do you want for the output?",
            from_filetype
        );
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
    };

    remove_keyboard_from(&bot, &q).await?;

    if let Some(from_filetype) = q.data {
        if FROM_FILETYPES.contains(&from_filetype.as_str()) {
            let next_state = State::ReceiveToFiletype {
                from_filetype: from_filetype.clone(),
            };

            make_success_msg(&from_filetype).send().await?;
            dialogue.update(next_state).await?;
        } else {
            make_fail_msg().send().await?;
        }
    } else {
        make_fail_msg().send().await?;
    }

    Ok(())
}

async fn receive_to_filetype(
    bot: Bot,
    q: CallbackQuery,
    dialogue: MyDialogue,
    from_filetype: String,
) -> HandlerResult {
    bot.answer_callback_query(q.id.clone()).send().await?;
    let chat_id = q.chat_id().context("No chat id found")?;

    let make_fail_msg = || {
        let keyboard = make_to_keyboard();

        let text = format!("What format do you want for the output?");
        bot.send_message(chat_id, text).reply_markup(keyboard)
    };

    let make_success_msg = |from_filetype| {
        let text = format!(
            "The output format is set to <b>{}</b>. \
             Now send me the file to be converted.",
            from_filetype
        );
        bot.send_message(chat_id, text).parse_mode(ParseMode::Html)
    };

    remove_keyboard_from(&bot, &q).await?;

    if let Some(to_filetype) = q.data {
        if TO_FILETYPES.contains(&to_filetype.as_str()) {
            let next_state = State::ReceiveInputFile {
                from_filetype,
                to_filetype: to_filetype.clone(),
            };

            make_success_msg(&to_filetype).send().await?;
            dialogue.update(next_state).await?;
        } else {
            make_fail_msg().send().await?;
        }
    } else {
        make_fail_msg().send().await?;
    }

    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
struct ConvertRequest {
    chat_id: i64,
    #[serde(with = "serde_bytes")]
    file: Vec<u8>,
    file_id: String,
    from_filetype: String,
    to_filetype: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum ConvertResponse {
    Success {
        chat_id: i64,
        #[serde(with = "serde_bytes")]
        file: Vec<u8>,
        to_filetype: String,
    },
    Failure {
        chat_id: i64,
        error_msg: String,
    },
}

async fn receive_input_file(
    bot: Bot,
    msg: Message,
    dialogue: MyDialogue,
    amqp_conn: Arc<lapin::Connection>,
    (from_filetype, to_filetype): (String, String),
) -> HandlerResult {
    let make_fail_msg = || {
        let keyboard = make_to_keyboard();

        let text = format!("Send me the file to be converted.");
        bot.send_message(msg.chat.id, text).reply_markup(keyboard)
    };

    let make_success_msg = || {
        bot.send_message(msg.chat.id, "The conversion is being performed ...")
            .parse_mode(ParseMode::Html)
    };

    if let Some(doc) = msg.document() {
        info!(
            "Received document with name {:?} and id {}",
            doc.file_name, doc.file_id
        );

        /* Download file to disk */
        // Not really file path on the FS, but this is how Telegram name their API
        let TgFile { file_path, .. } = bot.get_file(&doc.file_id).send().await?;

        let input_file_path = path_for_input_file(&doc.file_id);

        // Create base path for the input file
        tokio::fs::create_dir_all(
            input_file_path
                .parent()
                .context("No parent path for input_file_path")?,
        )
        .await?;

        // Download the file and sync
        let mut file = File::create(&input_file_path).await?;
        bot.download_file(&file_path, &mut file).await?;
        file.sync_all().await?;

        info!(
            "Downloaded document with name {:?} and id {}",
            doc.file_name, doc.file_id
        );

        make_success_msg().send().await?;
        dialogue.update(State::Start).await?;

        /* Send to job queue */
        let binary = tokio::fs::read(&input_file_path).await?;
        let channel = amqp_conn.create_channel().await?;

        // Create request and convert to BSON
        let req = {
            let req = ConvertRequest {
                chat_id: msg.chat.id.0,
                file: binary,
                file_id: doc.file_id.clone(),
                from_filetype,
                to_filetype,
            };
            bson::to_vec(&req)?
        };

        // Send to queue
        channel
            .basic_publish(
                "",
                "pandoc-bot-jobs",
                BasicPublishOptions::default(),
                &req,
                BasicProperties::default(),
            )
            .await?
            .await?;
    } else {
        make_fail_msg().send().await?;
    }

    Ok(())
}

const FROM_FILETYPES: &[&str] = &["markdown"];
const TO_FILETYPES: &[&str] = &["pdf", "latex", "docx", "odt"];

fn filetype_to_extension(filetype: &str) -> &'static str {
    match filetype {
        "markdown" => "md",
        "pdf" => "pdf",
        "latex" => "tex",
        "docx" => "docx",
        "odt" => "odt",
        _ => "txt",
    }
}

/// Convert array of `&str` into a keyboard
fn make_keyboard(contents: &[&str], num_per_row: usize) -> InlineKeyboardMarkup {
    let mut keyboard: Vec<Vec<InlineKeyboardButton>> = vec![];
    for filetypes in contents.chunks(num_per_row) {
        let row = filetypes
            .iter()
            .map(|&version| InlineKeyboardButton::callback(version.to_owned(), version.to_owned()))
            .collect();

        keyboard.push(row);
    }
    InlineKeyboardMarkup::new(keyboard)
}

fn make_from_keyboard() -> InlineKeyboardMarkup {
    make_keyboard(FROM_FILETYPES, 3)
}

fn make_to_keyboard() -> InlineKeyboardMarkup {
    make_keyboard(TO_FILETYPES, 3)
}

/// Remove keyboard from `CallbackQuery`
async fn remove_keyboard_from(bot: &Bot, query: &CallbackQuery) -> Result<()> {
    if let (Some(chat_id), Some(message)) = (&query.chat_id(), &query.message) {
        info!("Removing keyboard from {chat_id:?}, {message:?}");

        let mut req = bot.edit_message_reply_markup(*chat_id, message.id);
        req.reply_markup = None;
        req.send().await?;
    } else {
        info!("No chat_id or no message");
    }

    Ok(())
}

/// Defaults to `./inputs/<file_id>`.
/// If the env var is defined, then `$INPUT_BASE_PATH/inputs/<file_id>`.
fn path_for_input_file<S: AsRef<str>>(file_id: S) -> PathBuf {
    let mut path = env::var("INPUT_BASE_PATH")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from("inputs"));
    path.push(file_id.as_ref());
    path
}

fn path_for_persistent_state() -> PathBuf {
    if let Ok(path) = env::var("STATE_PATH") {
        PathBuf::from(path)
    } else {
        PathBuf::from("./")
    }
}
