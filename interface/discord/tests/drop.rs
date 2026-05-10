use std::sync::Arc;

use discord::Discord;

fn token() -> Option<String> {
    std::env::var("DISCORD_TOKEN").ok()
}

#[test]
fn messenger_drop_frees_memory() {
    let Some(token) = token() else {
        panic!("SKIPPED: Token wasn't provided");
    };
    let messenger = Discord::new_messenger(&token);
    let weak = Arc::downgrade(&messenger);

    drop(messenger);

    assert!(weak.upgrade().is_none());
}

#[test]
fn messenger_drop_frees_streams() {
    let Some(token) = token() else {
        panic!("SKIPPED: Token wasn't provided");
    };
    smol::block_on(async {
        let messenger = Discord::new_messenger(&token);
        let weak = Arc::downgrade(&messenger);

        let q = messenger.clone().arc_query().unwrap();
        let t = messenger.clone().arc_text().unwrap();
        let v = messenger.clone().arc_voice().unwrap();

        let _q_stream = q.listen().await.unwrap();
        let _t_stream = t.listen().await.unwrap();
        let _v_stream = v.listen().await.unwrap();

        drop(messenger);
        drop(_q_stream);
        drop(_t_stream);
        drop(_v_stream);

        assert!(weak.upgrade().is_none());
    });
}
