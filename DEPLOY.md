# Deployment Manual — Vol Harvest Bot v5

**Server:** AWS Ireland EC2 (`54.155.125.66`)
**User:** `ubuntu`
**SSH key:** `C:/dev/AWS pem/For my PC.pem`
**Repo:** `https://github.com/anescaper/My-First-Bot.git`
**Bot path:** `/home/ubuntu/polybot/bot/`
**Python:** 3.12.3

---

## Pre-deployment Checklist

Before ANY deployment, run these checks in order. Do NOT skip steps.

### Step 1: Check what is running

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "echo '=== PYTHON ===' && ps aux | grep main.py | grep -v grep; \
   echo '=== DOCKER ===' && docker ps --format '{{.Names}} {{.Status}}' 2>/dev/null; \
   echo '=== POLYBOT ===' && ps aux | grep polybot | grep -v grep | grep -v docker"
```

**Expected:** Nothing running (empty output for all three).
**If something IS running:** STOP. Ask the user before killing anything.

### Step 2: Check open orders on Polymarket

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "cd /home/ubuntu/polybot && python3 -c \"
import sys; sys.path.insert(0, 'bot')
from client import create_client
client = create_client()
orders = client.get_orders()
live = [o for o in orders if o.get('status','').upper() == 'LIVE']
print(f'Live orders on Polymarket: {len(live)}')
\" 2>&1 | grep -v 'HTTP Request'"
```

**Expected:** 0 live orders (or a known number from previous session).
**If orders exist:** Ask the user — they may want to cancel them first or leave them.

### Step 3: Check database state

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "if [ -f /home/ubuntu/polybot/bot.db ]; then \
     sqlite3 /home/ubuntu/polybot/bot.db \"
       SELECT 'orders: ' || COUNT(*) || ' open' FROM orders WHERE status='open';
       SELECT 'positions: ' || COUNT(*) || ' active' FROM positions WHERE status IN ('open','exiting');
     \"; \
   else echo 'No database (clean start)'; fi"
```

**Expected for clean deploy:** "No database (clean start)".
**If active positions exist:** STOP. The bot has unmanaged positions. Ask the user.

---

## Deployment Steps

### Step 4: Pull latest code

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "cd /home/ubuntu/polybot && git pull origin main"
```

Verify the commit matches what you expect:

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "cd /home/ubuntu/polybot && git log --oneline -1"
```

### Step 5: Verify config before starting

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "grep -E 'BUY_PRICE|SELL_TARGET|ASSETS|LUCKY_SETTLEMENT|MAX_OPEN|BUDGET_TOTAL' \
   /home/ubuntu/polybot/bot/config.py"
```

Read the output. Confirm with the user that these values are correct.
**Do NOT start the bot if config values are unexpected.**

### Step 6: Verify secrets exist

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "ls /home/ubuntu/polybot/secrets/"
```

**Required files:**
- `polymarket_api_key`
- `polymarket_api_secret`
- `polymarket_passphrase`
- `polymarket_private_key`
- `polymarket_funder_address`

### Step 7: Start the bot

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "cd /home/ubuntu/polybot && nohup python3 -u bot/main.py >> bot.log 2>&1 &"
```

### Step 8: Verify single instance

Wait 3 seconds, then:

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "ps aux | grep main.py | grep -v grep | wc -l"
```

**Expected:** Exactly `1`.
**If more than 1:** Kill ALL and restart. Only one instance must run.

### Step 9: Check startup logs

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "grep -v 'HTTP Request' /home/ubuntu/polybot/bot.log | tail -20"
```

**Expected output includes:**
- `VOL HARVEST BOT` banner with correct config values
- `ClobClient initialized (allowances set)`
- `Discovered N new rounds`
- `Placed N pre-orders`
- NO errors

---

## Stopping the Bot

### Step 1: Find the process

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "ps aux | grep main.py | grep -v grep"
```

### Step 2: Kill it

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "pkill -f 'bot/main.py'"
```

### Step 3: Confirm it stopped

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "ps aux | grep main.py | grep -v grep"
```

**Expected:** No output.

---

## Restarting the Bot (e.g., after config change)

1. **Stop** — follow "Stopping the Bot" above
2. Wait 2 seconds
3. **Check** — follow Steps 1-3 of Pre-deployment Checklist
4. **Start** — follow Steps 7-9 of Deployment Steps

**NEVER start a new instance without confirming the old one is dead.**

---

## Updating Config on Server

1. Edit the file on the server (or push from git)
2. **Show the user** the new config values before restarting
3. Only restart after user confirms

```bash
# Example: edit config remotely
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "cat /home/ubuntu/polybot/bot/config.py"
```

---

## Wiping the Database

Only do this when the user explicitly asks, or when starting completely fresh.

```bash
ssh -i "C:/dev/AWS pem/For my PC.pem" ubuntu@54.155.125.66 \
  "rm -f /home/ubuntu/polybot/bot.db /home/ubuntu/polybot/bot.db-wal /home/ubuntu/polybot/bot.db-shm"
```

**WARNING:** This forgets all open orders and positions. If the bot had live orders on Polymarket, they become orphaned (still live but unmanaged). Cancel them first via Step 2 of Pre-deployment Checklist.

---

## Rules

1. **One instance only.** Never run two bot processes. Always check before starting.
2. **No silent restarts.** Always tell the user before stopping or starting the bot.
3. **No DB writes outside the bot.** Never run UPDATE/DELETE on bot.db while the bot is running.
4. **No Docker.** The old Docker bot (`polybot-fv`) is removed. Do not start it.
5. **Config changes require restart.** The bot reads config at startup only.
6. **Check Polymarket API, not just DB.** The DB can be stale. Use `client.get_orders()` for ground truth.
7. **Commit before deploy.** Code on the server must match a commit on `main`.

---

## File Layout

```
/home/ubuntu/polybot/
├── bot/                  # Python bot (this repo)
│   ├── main.py           # Entry point
│   ├── config.py         # All tunable parameters
│   ├── client.py         # Polymarket API wrapper
│   ├── orders.py         # BUY order placement + fill detection
│   ├── exits.py          # SELL placement + exit logic
│   ├── discovery.py      # Round discovery from Gamma API
│   ├── db.py             # SQLite database layer
│   ├── models.py         # Data classes
│   ├── cleanup.py        # Stale round cleanup
│   └── signals.py        # Competitor signal tracking
├── secrets/              # API keys (NOT in git)
├── bot.db                # SQLite database (created at runtime)
├── bot.log               # Log file (append-only)
├── crates/               # Old Rust bot (unused)
├── python/               # Old Python strategy service (unused)
├── Dockerfile            # Old Docker config (unused)
└── docker-compose.yml    # Old Docker config (unused)
```
