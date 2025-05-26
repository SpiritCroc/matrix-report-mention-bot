use config::{Config, Value};
use log::{debug, info, error};
use url::Url;
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    authentication::matrix::MatrixSession,
    Client, Room, RoomState,
    ruma::events::{
        room::message::{
            MessageType, OriginalSyncRoomMessageEvent,
            RoomMessageEventContent,
        },
        reaction::ReactionEventContent,
        relation::Annotation,
        Mentions,
    },
    ruma::{RoomId, OwnedRoomId},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

#[derive(Clone)]
struct BotContext {
    launched_ts: u128,
    bot_mxid: String,
    bot_mxid_http_escaped: String,
    watched_rooms: Vec<OwnedRoomId>,
    watched_test_rooms: Vec<OwnedRoomId>,
    report_rooms: Vec<OwnedRoomId>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let config = Config::builder()
        .add_source(config::File::with_name("config.yaml"))
        .build()
        .unwrap();

    let hs = config.get::<String>("login.homeserver_url").expect("Homeserver url missing in config");
    let hs_url = Url::parse(&hs).expect("Invalid homeserver url");
    let mxid = config.get::<String>("login.mxid").expect("Bot mxid missing in config");
    let password = config.get::<String>("login.password").expect("Password missing in config");

    let report_rooms = config.get_array("bot.report_rooms")
        .expect("Missing bot.report_rooms in config")
        .into_iter()
        .map(Value::into_string)
        .map(Result::unwrap_or_default)
        .map(RoomId::parse)
        .map(|room_id| room_id.expect("Invalid roomId in bot.report_rooms"))
        .collect();

    let watched_rooms = config.get_array("bot.watched_rooms")
        .expect("Missing bot.watched_rooms in config")
        .into_iter()
        .map(Value::into_string)
        .map(Result::unwrap_or_default)
        .map(RoomId::parse)
        .map(|room_id| room_id.expect("Invalid roomId in bot.watched_rooms"))
        .collect();

    let watched_test_rooms = config.get_array("bot.watched_test_rooms")
        .unwrap_or_default()
        .into_iter()
        .map(Value::into_string)
        .map(Result::unwrap_or_default)
        .map(RoomId::parse)
        .map(|room_id| room_id.expect("Invalid roomId in bot.watched_test_rooms"))
        .collect();

    let data_dir = dirs::data_dir().expect("no data_dir directory found").join("matrix-report-mention-bot");
    let db_path = data_dir.join("db");
    let session_path = data_dir.join("session");

    // For mention detection in formatted content
    let bot_mxid_http_escaped = mxid.replace("@", "%40").replace(":", "%3A");

    let bot_context = BotContext {
        launched_ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        bot_mxid: mxid.clone(),
        bot_mxid_http_escaped: bot_mxid_http_escaped.clone(),
        watched_rooms,
        watched_test_rooms,
        report_rooms,
    };

    debug!("Data dir configured at {}", data_dir.to_str().unwrap_or_default());
    debug!("Logging into {hs_url} as {mxid} ({bot_mxid_http_escaped})...");

    let client = Client::builder()
        .homeserver_url(&hs_url)
        .sqlite_store(&db_path, None)
        .build()
        .await?;

    if session_path.exists() {
        info!("Restoring old login...");
        let serialized_session = fs::read_to_string(session_path).await?;
        let user_session: MatrixSession = serde_json::from_str(&serialized_session)?;
        client.restore_session(user_session).await?;
    } else {
        info!("Doing a fresh login...");

        let device_name = config.get::<String>("login.device_name").unwrap_or(String::from("report-mention-bot"));

        let matrix_auth = client.matrix_auth();
        let login_response = matrix_auth
            .login_username(&mxid, &password)
            .initial_device_display_name(&device_name)
            .await?;

        info!("Logged in as {}", login_response.device_id);

        let user_session = matrix_auth.session().expect("A logged-in client should have a session");
        let serialized_session = serde_json::to_string(&user_session)?;
        fs::write(session_path, serialized_session).await?;
    }

    client.add_event_handler_context(bot_context);

    // Sync once without message handler to not deal with old messages
    let sync_response = client.sync_once(SyncSettings::default()).await.unwrap();
    info!("Initial sync finished with token {}, start listening for events", sync_response.next_batch);

    // Actual message handling and sync loop
    client.add_event_handler(handle_message);
    client.sync(SyncSettings::default().token(sync_response.next_batch)).await?;

    Ok(())
}

async fn handle_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    bot_context: Ctx<BotContext>,
) {
    if room.state() != RoomState::Joined {
        return;
    }
    if event.sender == room.own_user_id() {
        return;
    }
    let is_watched = bot_context.watched_rooms.clone().into_iter().any(|r| r == room.room_id());
    let is_test = !is_watched && bot_context.watched_test_rooms.clone().into_iter().any(|r| r == room.room_id());
    if !is_watched && !is_test {
        return;
    }
    let MessageType::Text(text_content) = event.clone().content.msgtype else {
        return;
    };

    if u128::from(event.origin_server_ts.0) < bot_context.0.launched_ts - 10_000 {
        info!("Ignore message in the past: {} in {}", event.event_id, room.room_id());
        return
    }

    let bot_mxid = bot_context.0.bot_mxid;
    let bot_mxid_escaped = bot_context.0.bot_mxid_http_escaped;

    if text_content.body.contains(&bot_mxid) ||
        text_content.formatted.map(|f|
            f.body.contains(&bot_mxid) || f.body.contains(&bot_mxid_escaped)
        ).unwrap_or(false) ||
        event.content.mentions.map(|m|
            m.user_ids.contains(room.own_user_id())
        ).unwrap_or(false)
    {
        let orig_sender = event.sender;
        let orig_url = room.room_id().matrix_to_event_uri(event.event_id.clone());
        let mut reported = false;
        for report_room_id in bot_context.0.report_rooms {
            let report_room = room.client().get_room(&report_room_id);
            match report_room {
                None => error!("Failed to retrieve report room {report_room_id} from client"),
                Some(report_room) => {
                    let content = if is_test {
                        let msg = format!("I was pinged by {orig_sender} at {orig_url}, which is a test room so I won't bother you with a room ping this time");
                        RoomMessageEventContent::notice_markdown(msg)
                    } else {
                        let msg = format!("@room: I was pinged by {orig_sender} at {orig_url}");
                        RoomMessageEventContent::text_markdown(msg)
                            .add_mentions(Mentions::with_room_mention())
                    };
                    if let Err(e) = report_room.send(content).await {
                        error!("Failed to report message from {} at {}: {}", orig_sender, orig_url, e);
                    } else {
                        info!("Successfully reported message from {} at {} to {}", orig_sender, orig_url, report_room_id);
                        reported = true;
                    }
                }
            }
        }
        if reported {
            // Send reaction to signal we reported it
            let reaction = ReactionEventContent::new(
                Annotation::new(
                    event.event_id,
                    "📨".to_owned(),
                )
            );
            if let Err(e) = room.send(reaction).await {
                error!("Failed to send ack reaction: {}", e);
            }
        } else {
            error!("Failed to report to any room, not sending any ack reaction");
        }
    }
}
