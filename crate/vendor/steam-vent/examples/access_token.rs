use std::env::args;
use std::error::Error;
use steam_vent::auth::{
    AuthConfirmationHandler, ConsoleAuthConfirmationHandler, DeviceConfirmationHandler,
    FileGuardDataStore,
};
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetOwnedGames_Request;
use steam_vent::{Connection, ConnectionTrait, ServerList};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let mut args = args().skip(1);
    let account = args.next().expect("no account");
    let password = args.next().expect("no password");
    let access_token = args.next();

    let server_list = ServerList::discover().await?;
    let connection = match access_token {
        Some(access_token) => match Connection::access(&server_list, &account, &access_token).await
        {
            Ok(connection) => Some(connection),
            Err(error) => {
                eprintln!("connection using access token failed: {error}");
                None // Fallback to password
            }
        },
        None => None,
    };

    let connection = if let Some(connection) = connection {
        connection
    } else {
        let connection = Connection::login(
            &server_list,
            &account,
            &password,
            FileGuardDataStore::user_cache(),
            ConsoleAuthConfirmationHandler::default().or(DeviceConfirmationHandler),
        )
        .await?;

        println!(
            "access token for future use: {:?}",
            connection.access_token()
        );

        connection
    };

    println!("requesting games");

    let req = CPlayer_GetOwnedGames_Request {
        steamid: Some(connection.steam_id().into()),
        include_appinfo: Some(true),
        include_played_free_games: Some(true),
        ..CPlayer_GetOwnedGames_Request::default()
    };
    let games = connection.service_method(req).await?;
    println!(
        "{} owns {} games",
        connection.steam_id().steam3(),
        games.game_count()
    );
    for game in games.games {
        println!(
            "{}: {} {}",
            game.appid(),
            game.name(),
            game.playtime_forever()
        );
    }

    Ok(())
}
