use anyhow::{Context, Result};
use log::info;
use serde::{Deserialize, Serialize};
use teloxide::{
    dispatching::dialogue::{
        self, serializer::Json, ErasedStorage, GetChatId, SqliteStorage, Storage,
    },
    net::Download,
    prelude::*,
    types::{File as TgFile, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};

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
    log::info!("Starting dialogue bot...");

    let bot = Bot::from_env();

    let dialog_storage: MyStorage = SqliteStorage::open("./dialogue.sqlite3", Json)
        .await
        .context("Failed to open SqliteStorage")?
        .erase();

    Dispatcher::builder(
        bot,
        dialogue::enter::<Update, ErasedStorage<State>, State, _>()
            .branch(
                Update::filter_message()
                    .branch(dptree::case![State::Start].endpoint(start))
                    .branch(dptree::case![State::ReceiveFullName].endpoint(receive_full_name))
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
                    .branch(
                        dptree::case![State::ReceiveFromFiletype].endpoint(receive_from_filetype),
                    )
                    .branch(
                        dptree::case![State::ReceiveToFiletype { from_filetype }]
                            .endpoint(receive_to_filetype),
                    ),
            ),
    )
    .dependencies(dptree::deps![dialog_storage])
    .build()
    .setup_ctrlc_handler()
    .dispatch()
    .await;

    Ok(())
}

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

async fn receive_full_name(bot: Bot, msg: Message, dialogue: MyDialogue) -> HandlerResult {
    match msg.text() {
        Some(text) => {
            bot.send_message(msg.chat.id, "How old are you?")
                .send()
                .await?;
            dialogue
                .update(State::ReceiveAge {
                    full_name: text.into(),
                })
                .await?;
        }
        None => {
            bot.send_message(msg.chat.id, "Send me plain text.")
                .send()
                .await?;
        }
    }

    Ok(())
}

async fn receive_from_filetype(bot: Bot, q: CallbackQuery, dialogue: MyDialogue) -> HandlerResult {
    let chat_id = q.chat_id().context("No chat id found")?;

    let make_fail_msg = || {
        let keyboard = make_from_keyboard();
        bot.send_message(chat_id, "Tell me the type of the original document.")
            .reply_markup(keyboard)
    };

    let make_success_msg = |from_filetype| {
        let keyboard = make_to_keyboard();

        let text = format!(
            "The type of the original document is set to <code>{}</code>. \
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
    let chat_id = q.chat_id().context("No chat id found")?;

    let make_fail_msg = || {
        let keyboard = make_to_keyboard();

        let text = format!("What format do you want for the output?");
        bot.send_message(chat_id, text).reply_markup(keyboard)
    };

    let make_success_msg = |from_filetype| {
        let text = format!(
            "The output format is set to <code>{}</code>. \
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

async fn receive_input_file(
    bot: Bot,
    msg: Message,
    dialogue: MyDialogue,
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

        let TgFile { file_path, .. } = bot.get_file(&doc.file_id).send().await?;
        let mut file = tokio::fs::File::create(&doc.file_id).await?;

        bot.download_file(&file_path, &mut file).await?;

        info!(
            "Downloaded document with name {:?} and id {}",
            doc.file_name, doc.file_id
        );

        make_success_msg().send().await?;
        dialogue.update(State::Start).await?;
    } else {
        make_fail_msg().send().await?;
    }

    Ok(())
}

const FROM_FILETYPES: &[&str] = &["markdown", "asciidoc"];
const TO_FILETYPES: &[&str] = &["PDF", "LaTeX"];

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
