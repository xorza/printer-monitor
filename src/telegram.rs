use reqwest::Client;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile};

#[derive(Debug)]
pub struct Telegram {
    bot: Bot,
    chat_id: ChatId,
}

impl Telegram {
    pub fn new(client: Client, token: String, chat_id: ChatId) -> Self {
        Self {
            bot: Bot::with_client(token, client),
            chat_id,
        }
    }

    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    pub fn chat_id(&self) -> ChatId {
        self.chat_id
    }

    pub async fn send_photo(
        &self,
        jpeg: Vec<u8>,
        caption: &str,
        buttons: &[InlineKeyboardButton],
    ) -> Result<(), teloxide::RequestError> {
        let mut req = self
            .bot
            .send_photo(
                self.chat_id,
                InputFile::memory(jpeg).file_name("snapshot.jpg"),
            )
            .caption(caption);
        if !buttons.is_empty() {
            req = req.reply_markup(InlineKeyboardMarkup::new(vec![buttons.to_vec()]));
        }
        req.await?;
        Ok(())
    }
}
