<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
![Current version)](https://img.shields.io/badge/current_version-1.21.11-blue)

A tool to stress test Minecraft servers by simulating numerous bot connections, helping identify performance bottlenecks and stability issues.
![Screenshot_20250223_154536](https://github.com/user-attachments/assets/b1379060-e375-44dc-95a3-91f1f2fb52dd)

</div>

### How to Run

1.  **Disable Online Mode & Encryption:** Ensure that `online-mode=false` in your server's `server.properties` file. This is crucial for Botmark to connect as it simulates offline-mode clients. **(Warning: only do this in a test environment)**.
2.  **Run Botmark:** Use the following command in your terminal, adjusting the arguments as needed:

    ```bash
    ./botmark --ip <ip:port> --count <bot-count> [OPTIONS]
    ```

### Command-Line Arguments

Here's a detailed breakdown of all available command-line arguments:

- **`--ip <ip:port>` (Required):**

  - The IP address and port of the Minecraft server to stress test.
  - Example: `127.0.0.1:25565` or `example.com:25565`

- **`-c, --count <bot-count>` (Optional, Default: `1`):**

  - The number of bots to simulate connecting to the server.
  - Example: `--count 50`

- **`-d, --delay <delay>` (Optional, Default: `200`):**
  - The delay (in milliseconds) between each bot connection.
  - Example: `--delay 100` (100 milliseconds delay)

- **`-t, --timeout <delay>` (Optional, Default: `5000`):**
  - The timeout (in milliseconds) for each bot connection.
  - Example: `--delay 500` (500 milliseconds delay)
  
- **`--spam_message "<message>"` (Optional, Default: `Please do not spam!`):**
  - Will send a Chat message with the specified `spam_message_delay`.
  - Example: `--spam_message "Hello, I'm a Robot"`

- **`--spam_message_delay_min <delay>` (Optional, Default: `150`):**
  - The minimal delay (in milliseconds) between spam message.
  - Example: `--spam_message_delay_min 100` (100 milliseconds delay)
- **`--spam_message_delay_max <delay>` (Optional, Default: `350`):**
  - The maximal delay (in milliseconds) between spam message.
  - Example: `--spam_message_delay_max 100` (100 milliseconds delay)
- **`--spam_message_delay_max <delay>` (Optional, Default: `350`):**
  - The maximal delay (in milliseconds) between spam message.
  - Example: `--spam_message_delay_max 100` (100 milliseconds delay)
- **`--enable_rotation <false/true>` (Optional, Default: `true`):**
  - Enable random head rotations.
  - Example: `--enable_rotation true`
- **`--enable_swing <false/true>` (Optional, Default: `true`):**
  - Enable random arm swings.
  - Example: `--enable_swing true`