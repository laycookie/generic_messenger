> ⚠️ **Warning:** This project is currently in a rapid iteration prototype stage.
# Introduction
This project aims to create a lightweight, unified messaging application capable of connecting to any messaging platform you desire.

### Native support
- [x] Discord
- [ ] Slack
- [ ] Teams
- [ ] Signal
- [ ] Steam

# Technical details
The core of the project is divided into two main components: adapters and the application.

The application crate is responsible for handling the user interface and coordinating communication with the adapter crates to send and receive messages across different messaging backends.

Adapters crate exposes all messaging backends as a Messenger object, providing a unified interface for the application to interact with various platforms.
