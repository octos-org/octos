# Quick Start

This guide walks you through the essential steps to get Octos running.

## 1. Initialize Your Workspace

Navigate to your project directory and initialize Octos:

```bash
cd your-project
octos init
```

This creates a `.octos/` directory with default configuration, bootstrap files (AGENTS.md, SOUL.md, USER.md), and directories for memory, sessions, and skills.

## 2. Set Your API Key

Export at least one LLM provider key:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

Add this to your `~/.bashrc` or `~/.zshrc` for persistence. You can also use `octos auth login --provider openai` for OAuth-based login.

## 3. Check Setup

Verify everything is configured correctly:

```bash
octos status
```

This shows your config file location, active provider and model, API key status, and bootstrap file availability.

## 4. Start Chatting

Launch an interactive multi-turn conversation:

```bash
octos chat
```

Or send a single message and exit:

```bash
octos chat --message "Add a hello function to lib.rs"
```

## 5. Run the Gateway

To serve multiple messaging channels as a persistent daemon:

```bash
octos gateway
```

This requires a `gateway` section in your config with at least one channel configured. See the [Configuration](configuration.md) chapter for details.

## 6. Launch the Web UI

If you built with the `api` feature, start the web dashboard:

```bash
octos serve
```

Then open `http://localhost:8080` in your browser.
