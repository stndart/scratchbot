use crate::TELEGRAM_MAX_FILE_BYTES;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub media_group_id: Option<String>,
    #[serde(default)]
    pub photo: Vec<PhotoSize>,
    pub animation: Option<FilePayload>,
    pub video: Option<FilePayload>,
    pub document: Option<FilePayload>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub width: u32,
    pub height: u32,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilePayload {
    pub file_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingFile {
    pub file_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingItem {
    pub update_ids: Vec<i64>,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub media_group_id: Option<String>,
    pub body: String,
    pub files: Vec<IncomingFile>,
    pub too_large: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start,
    Space { name: Option<String> },
    Login { name: Option<String> },
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    Ignored,
    Command {
        chat_id: i64,
        user_id: i64,
        command: Command,
    },
    Item(IncomingItem),
}

impl IncomingItem {
    pub fn idempotency_key(&self) -> String {
        match &self.media_group_id {
            Some(group) => format!("tg:{}:album:{group}", self.chat_id),
            None => format!("tg:{}:{}", self.chat_id, self.message_id),
        }
    }
}

pub fn is_allowed(user_id: i64, allow: &[i64]) -> bool {
    allow.is_empty() || allow.contains(&user_id)
}

pub fn parse_command(text: &str) -> Option<Command> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }
    let (raw_cmd, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let cmd = raw_cmd
        .split('@')
        .next()
        .unwrap_or(raw_cmd)
        .to_ascii_lowercase();
    match cmd.as_str() {
        "/start" | "/help" => Some(Command::Start),
        "/space" => {
            let name = rest.trim();
            Some(Command::Space {
                name: (!name.is_empty()).then(|| name.to_owned()),
            })
        }
        "/login" => {
            let name = rest.trim();
            Some(Command::Login {
                name: (!name.is_empty()).then(|| name.to_owned()),
            })
        }
        other => Some(Command::Unknown(other.to_owned())),
    }
}

pub fn classify(update: &Update, allow: &[i64]) -> Classified {
    let Some(message) = update.message.as_ref() else {
        return Classified::Ignored;
    };
    let Some(from_id) = message.from.as_ref().map(|user| user.id) else {
        return Classified::Ignored;
    };
    if !is_allowed(from_id, allow) {
        return Classified::Ignored;
    }
    if let Some(text) = message.text.as_deref()
        && let Some(command) = parse_command(text)
    {
        return Classified::Command {
            chat_id: message.chat.id,
            user_id: from_id,
            command,
        };
    }
    let body = message
        .caption
        .clone()
        .or_else(|| message.text.clone())
        .unwrap_or_default();
    let mut files = Vec::new();
    let mut too_large = None;
    if let Some(photo) = largest_photo(&message.photo) {
        push_file(
            &mut files,
            &mut too_large,
            IncomingFile {
                file_id: photo.file_id.clone(),
                filename: "photo.jpg".into(),
                mime_type: "image/jpeg".into(),
                size: photo.file_size,
            },
        );
    } else if let Some(animation) = &message.animation {
        push_file(
            &mut files,
            &mut too_large,
            IncomingFile {
                file_id: animation.file_id.clone(),
                filename: animation
                    .file_name
                    .clone()
                    .unwrap_or_else(|| "animation.mp4".into()),
                mime_type: animation
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "video/mp4".into()),
                size: animation.file_size,
            },
        );
    } else if let Some(video) = &message.video {
        push_file(
            &mut files,
            &mut too_large,
            IncomingFile {
                file_id: video.file_id.clone(),
                filename: video
                    .file_name
                    .clone()
                    .unwrap_or_else(|| "video.mp4".into()),
                mime_type: video
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "video/mp4".into()),
                size: video.file_size,
            },
        );
    } else if let Some(document) = &message.document {
        push_file(
            &mut files,
            &mut too_large,
            IncomingFile {
                file_id: document.file_id.clone(),
                filename: document.file_name.clone().unwrap_or_else(|| "file".into()),
                mime_type: document
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into()),
                size: document.file_size,
            },
        );
    } else if body.trim().is_empty() {
        return Classified::Ignored;
    }
    Classified::Item(IncomingItem {
        update_ids: vec![update.update_id],
        chat_id: message.chat.id,
        user_id: from_id,
        message_id: message.message_id,
        media_group_id: message.media_group_id.clone(),
        body,
        files,
        too_large,
    })
}

fn largest_photo(photos: &[PhotoSize]) -> Option<&PhotoSize> {
    photos
        .iter()
        .max_by_key(|photo| photo.width as u64 * photo.height as u64)
}

fn push_file(files: &mut Vec<IncomingFile>, too_large: &mut Option<String>, file: IncomingFile) {
    if file.size.is_some_and(|size| size > TELEGRAM_MAX_FILE_BYTES) {
        *too_large = Some(file.filename);
        return;
    }
    files.push(file);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TELEGRAM_MAX_FILE_BYTES;

    fn parse(json: &str) -> Update {
        serde_json::from_str(json).unwrap()
    }

    fn item_of(update: &Update, allow: &[i64]) -> IncomingItem {
        match classify(update, allow) {
            Classified::Item(item) => item,
            other => panic!("expected item, got {other:?}"),
        }
    }

    #[test]
    fn allowlist_rejects_unknown_senders_unless_open() {
        assert!(!is_allowed(99, &[1, 2, 42]));
        assert!(is_allowed(42, &[1, 2, 42]));
        assert!(is_allowed(99, &[]));
    }

    #[test]
    fn space_command_is_not_ingested() {
        let update = parse(
            r#"{
                "update_id": 99,
                "message": {
                    "message_id": 4,
                    "from": {"id": 42, "is_bot": false, "first_name": "S"},
                    "chat": {"id": 42, "type": "private"},
                    "text": "/space@ScratchwallBot My Memes"
                }
            }"#,
        );
        assert_eq!(
            classify(&update, &[]),
            Classified::Command {
                chat_id: 42,
                user_id: 42,
                command: Command::Space {
                    name: Some("My Memes".into())
                }
            }
        );
        assert_eq!(parse_command("/start"), Some(Command::Start));
        assert_eq!(parse_command("/space"), Some(Command::Space { name: None }));
        assert_eq!(
            parse_command("/login Svyat"),
            Some(Command::Login {
                name: Some("Svyat".into())
            })
        );
        assert_eq!(parse_command("/login"), Some(Command::Login { name: None }));
    }

    #[test]
    fn photo_caption_maps_to_ingest_fields() {
        let update = parse(
            r#"{
                "update_id": 100,
                "message": {
                    "message_id": 5,
                    "from": {"id": 42, "is_bot": false, "first_name": "S"},
                    "chat": {"id": 42, "type": "private"},
                    "caption": "funny #meme",
                    "photo": [
                        {"file_id": "small", "width": 90, "height": 90, "file_size": 100},
                        {"file_id": "big", "width": 1280, "height": 720, "file_size": 50000}
                    ]
                }
            }"#,
        );
        let item = item_of(&update, &[42]);
        assert_eq!(item.body, "funny #meme");
        assert_eq!(item.user_id, 42);
        assert_eq!(item.idempotency_key(), "tg:42:5");
        assert_eq!(item.files.len(), 1);
        assert_eq!(item.files[0].file_id, "big");
        assert_eq!(item.files[0].filename, "photo.jpg");
        assert_eq!(item.files[0].mime_type, "image/jpeg");
        assert!(item.too_large.is_none());
        assert_eq!(classify(&update, &[7]), Classified::Ignored);
    }

    #[test]
    fn text_only_is_a_body_memo() {
        let update = parse(
            r#"{
                "update_id": 101,
                "message": {
                    "message_id": 6,
                    "from": {"id": 42, "is_bot": false, "first_name": "S"},
                    "chat": {"id": 42, "type": "private"},
                    "text": "just a note"
                }
            }"#,
        );
        let item = item_of(&update, &[42]);
        assert_eq!(item.body, "just a note");
        assert!(item.files.is_empty());
    }

    #[test]
    fn stickers_and_voice_are_skipped() {
        let update = parse(
            r#"{
                "update_id": 102,
                "message": {
                    "message_id": 7,
                    "from": {"id": 42, "is_bot": false, "first_name": "S"},
                    "chat": {"id": 42, "type": "private"},
                    "sticker": {"file_id": "sticker", "width": 512, "height": 512, "is_animated": false, "is_video": false, "type": "regular", "emoji": "😀", "set_name": "x", "file_size": 12}
                }
            }"#,
        );
        assert_eq!(classify(&update, &[42]), Classified::Ignored);
    }

    #[test]
    fn files_over_20_mib_are_marked_too_large() {
        let size = TELEGRAM_MAX_FILE_BYTES + 1;
        let update = parse(&format!(
            r#"{{
                "update_id": 103,
                "message": {{
                    "message_id": 8,
                    "from": {{"id": 42, "is_bot": false, "first_name": "S"}},
                    "chat": {{"id": 42, "type": "private"}},
                    "caption": "huge",
                    "document": {{"file_id": "doc", "file_name": "clip.mp4", "mime_type": "video/mp4", "file_size": {size}}}
                }}
            }}"#
        ));
        let item = item_of(&update, &[42]);
        assert_eq!(item.too_large.as_deref(), Some("clip.mp4"));
        assert!(item.files.is_empty());
    }
}
