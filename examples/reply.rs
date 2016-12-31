extern crate telebot;
extern crate tokio_core;
extern crate futures;

use telebot::bot;
use tokio_core::reactor::Core;
use futures::stream::Stream;
use futures::Future;
use std::env;

// import all available functions
use telebot::functions::*;

fn main() {
    // Create a new tokio core
    let mut lp = Core::new().unwrap();

    // Create the bot
    let bot = bot::RcBot::new(lp.handle(), &env::var("TELEGRAM_BOT_KEY").unwrap())
        .update_interval(200);

    // Register a reply command which answer a message
    let handle = bot.new_cmd("/reply")
        .and_then(|(bot, msg)| {
            let mut text = msg.text.unwrap().clone();
            if text.is_empty() {
                text = "<empty>".into();
            }

            bot.send_message(msg.chat.id, text).send()
        });

    bot.register(handle);
 
    // enter the main loop
    bot.run(&mut lp).unwrap();
}