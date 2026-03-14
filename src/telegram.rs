use reqwest::Client;
use std::path::Path;
use teloxide::prelude::*;
use teloxide::types::InputFile;

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
        photo_path: &Path,
        caption: &str,
    ) -> Result<(), teloxide::RequestError> {
        self.bot
            .send_photo(self.chat_id, InputFile::file(photo_path))
            .caption(caption)
            .await?;
        Ok(())
    }
}
