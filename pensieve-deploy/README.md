# Pensieve Deployment

Production deployment configuration for Pensieve Nostr indexer.

## Server Requirements

- **OS**: Debian 12 (Bookworm) or Ubuntu 24.04 LTS
- **CPU**: 8+ cores recommended (signature verification is parallelizable)
- **RAM**: 64+ GB (ClickHouse loves RAM)
- **Storage**:
  - Fast SSD/NVMe for ClickHouse + RocksDB (`/data`)
  - HDD acceptable for archive segments (`/archive`)

### Reference Setup (Hetzner AX102)

```
CPU:     AMD Ryzen 9 3900 (12c/24t)
RAM:     128 GB
NVMe:    2x 1.92 TB (RAID 0) → /data (~3.8 TB)
HDD:     1x 6 TB             → /archive
```

---

## Fresh Server Setup

### 1. System Packages

```bash
# Update system
apt update && apt upgrade -y

# Install Docker
apt install -y ca-certificates curl gnupg
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
chmod a+r /etc/apt/keyrings/docker.gpg

echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
  https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  tee /etc/apt/sources.list.d/docker.list > /dev/null

apt update
apt install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin

# Build tools (for compiling Rust)
apt install -y build-essential pkg-config libssl-dev libclang-dev protobuf-compiler

# Other tools
apt install -y rclone htop iotop tmux git
```

### 2. Create Application User

```bash
useradd -m -s /bin/bash pensieve
usermod -aG docker pensieve

# Grant sudo access (for systemd operations)
echo "pensieve ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/pensieve

# Set up SSH key (optional, for deployments)
mkdir -p /home/pensieve/.ssh
cp ~/.ssh/authorized_keys /home/pensieve/.ssh/
chown -R pensieve:pensieve /home/pensieve/.ssh
chmod 700 /home/pensieve/.ssh
chmod 600 /home/pensieve/.ssh/authorized_keys
```

### 3. Install Rust (as pensieve user)

```bash
su - pensieve

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Accept defaults (option 1)

# Reload shell environment
source ~/.cargo/env

# Verify installation
rustc --version
cargo --version
```

### 4. Set Up HDD (if not done during OS install)

If the HDD wasn't configured during `installimage`:

```bash
# Check drive names
lsblk

# Partition and format HDD (adjust /dev/sda if different)
parted /dev/sda --script mklabel gpt
parted /dev/sda --script mkpart primary xfs 0% 100%
mkfs.xfs -L archive /dev/sda1

# Create mount point
mkdir -p /archive

# Add to fstab
echo 'LABEL=archive /archive xfs defaults,noatime 0 2' >> /etc/fstab

# Mount
mount /archive
```

### 5. Create Directory Structure

```bash
# Data directories (on SSD/NVMe)
mkdir -p /data/clickhouse
mkdir -p /data/rocksdb

# Archive directory (on HDD)
mkdir -p /archive/segments

# Set ownership
chown -R pensieve:pensieve /data /archive
```

### 6. Clone Repository

```bash
echo "pensieve ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/pensieve
su - pensieve
git clone https://github.com/andotherstuff/pensieve.git
cd ~/pensieve/pensieve-deploy
cp env.example .env
# Edit .env with your settings
nano .env
```

### 7. Configure Storage Box (for remote archive)

```bash
# Configure rclone for Hetzner Storage Box
rclone config

# Choose: New remote
# Name: storagebox
# Type: sftp (or webdav)
# Host: <your-storagebox>.your-storagebox.de
# User: <your-user>
# Port: 23 (for SFTP)
# Pass: <your-password>

# Test connection
rclone ls storagebox:

# Create archive directory on storage box
rclone mkdir storagebox:pensieve/archive
```

### 8. Install Systemd Services

```bash
# Copy service files
sudo cp ~/pensieve/pensieve-deploy/systemd/*.service /etc/systemd/system/
sudo cp ~/pensieve/pensieve-deploy/systemd/*.timer /etc/systemd/system/

# Reload systemd
sudo systemctl daemon-reload

# Enable and start services
sudo systemctl enable pensieve
sudo systemctl start pensieve

# Enable archive sync timer (syncs to Storage Box)
sudo systemctl enable archive-sync.timer
sudo systemctl start archive-sync.timer
```

### 9. Initialize ClickHouse Schema

```bash
# Wait for ClickHouse to be ready
docker compose exec clickhouse clickhouse-client --query "SELECT 1"

# Schema is auto-initialized from mounted SQL file
# Verify:
docker compose exec clickhouse clickhouse-client \
  --query "SELECT name FROM system.tables WHERE database = 'nostr'"
```

---

## Directory Layout

```
/data/                      # SSD/NVMe mount
├── clickhouse/             # ClickHouse data files
└── rocksdb/                # Deduplication index

/archive/                   # HDD mount (local buffer)
└── segments/               # Notepack segment files
    ├── segment-000001.notepack
    ├── segment-000002.notepack
    └── ...

/home/pensieve/pensieve/              # Repository clone
├── crates/                           # Rust crates
├── docs/                             # Documentation
└── pensieve-deploy/                  # Deployment config
    ├── docker-compose.yml
    ├── .env
    ├── caddy/
    │   └── Caddyfile                 # Reverse proxy config
    ├── clickhouse/
    │   └── config.xml                # ClickHouse tuning
    ├── grafana/
    │   └── provisioning/             # Auto-configured dashboards
    ├── prometheus/
    │   └── prometheus.yml            # Scrape targets
    ├── systemd/
    │   ├── pensieve.service
    │   ├── archive-sync.service
    │   └── archive-sync.timer
    └── scripts/
        ├── deploy.sh
        └── sync-archive.sh
```

---

## Operations

### Start/Stop Services

```bash
# Using systemd (recommended)
sudo systemctl start pensieve
sudo systemctl stop pensieve
sudo systemctl status pensieve

# Or directly with docker compose
cd ~/pensieve/pensieve-deploy
docker compose up -d
docker compose down
docker compose logs -f
```

### View Logs

```bash
# ClickHouse logs
docker compose logs -f clickhouse

# All service logs
journalctl -u pensieve -f
```

### Check ClickHouse

```bash
# Connect to ClickHouse CLI
docker compose exec clickhouse clickhouse-client

# Quick stats
docker compose exec clickhouse clickhouse-client \
  --query "SELECT count() FROM nostr.events_local"

# Disk usage
docker compose exec clickhouse clickhouse-client \
  --query "SELECT formatReadableSize(sum(bytes_on_disk)) FROM system.parts WHERE database = 'nostr'"
```

### Manual Archive Sync

```bash
# Sync old segments to Storage Box
~/pensieve/pensieve-deploy/scripts/sync-archive.sh

# Check sync status
rclone size storagebox:pensieve/archive
```

### Run Backfill

```bash
# Build the backfill tool (on your dev machine)
cargo build --release -p pensieve-ingest --bin backfill-proto

# Or run from Docker
docker compose run --rm ingester backfill-proto \
  --s3-bucket your-bucket \
  --s3-prefix nostr/segments/ \
  -o /archive/segments \
  --rocksdb-path /data/rocksdb \
  --clickhouse-url http://clickhouse:8123
```

---

## Monitoring

### Accessing Services

All services are exposed through Caddy reverse proxy:

| Service | Local URL | Production URL |
|---------|-----------|----------------|
| Grafana | http://localhost/grafana/ | https://your-domain.com/grafana/ |
| Prometheus | http://localhost:9090 (direct) | Internal only |
| Pensieve API | http://localhost/api/ (when ready) | https://your-domain.com/api/ |

**Grafana Login**: `admin` / `admin` (or value of `GRAFANA_PASSWORD` in `.env`)

### Caddy Reverse Proxy

Caddy provides:
- **Auto-HTTPS**: Automatic Let's Encrypt certificates for production domains
- **Path-based routing**: All services under one domain
- **Simple config**: `caddy/Caddyfile`

#### Local Development

```bash
# In .env
DOMAIN=localhost

# Access via HTTP
http://localhost/grafana/
```

#### Production with HTTPS

```bash
# In .env
DOMAIN=pensieve.example.com

# Ensure:
# 1. DNS A record points to your server's IP
# 2. Ports 80 and 443 are open in firewall
# 3. No other service is using port 80/443

# Access via HTTPS (auto-provisioned)
https://pensieve.example.com/grafana/
```

#### Firewall Setup

```bash
# If using ufw
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp

# If using iptables
sudo iptables -A INPUT -p tcp --dport 80 -j ACCEPT
sudo iptables -A INPUT -p tcp --dport 443 -j ACCEPT
```

#### Custom Routes

Edit `caddy/Caddyfile` to add or modify routes:

```caddyfile
# Uncomment to expose Prometheus UI
handle_path /prometheus/* {
    reverse_proxy prometheus:9090
}

# When pensieve-serve is ready
handle_path /api/* {
    reverse_proxy pensieve-serve:8080
}
```

After editing, reload Caddy:

```bash
docker compose exec caddy caddy reload --config /etc/caddy/Caddyfile
```

### Metrics Exposed by Backfill

The `backfill-proto` binary exposes Prometheus metrics on port 9091 (configurable with `--metrics-port`):

| Metric | Type | Description |
|--------|------|-------------|
| `backfill_running` | Gauge | 1 if running, 0 if stopped |
| `backfill_events_total` | Counter | Total events processed |
| `backfill_events_valid_total` | Counter | Valid events written |
| `backfill_events_duplicate_total` | Counter | Duplicates skipped |
| `backfill_events_invalid_total` | Counter | Invalid events |
| `backfill_files_total` | Counter | Files processed |
| `backfill_segments_sealed_total` | Counter | Segments sealed |
| `backfill_bytes_total{type="..."}` | Counter | Bytes by type |
| `backfill_events_per_second` | Gauge | Current processing rate |

### Linux Host Networking

On Linux, Docker doesn't provide `host.docker.internal` by default. The docker-compose
is already configured with `extra_hosts` to handle this:

```yaml
# Already in docker-compose.yml
extra_hosts:
  - "host.docker.internal:host-gateway"
```

This allows Prometheus to scrape metrics from backfill processes running on the host.

### Disk Usage

```bash
# Check all mounts
df -h

# Check specific directories
du -sh /data/clickhouse /data/rocksdb /archive/segments
```

### ClickHouse Performance

```sql
-- Recent queries
SELECT query, read_rows, elapsed FROM system.query_log
ORDER BY event_time DESC LIMIT 10;

-- Table sizes
SELECT
    table,
    formatReadableSize(sum(bytes_on_disk)) as size,
    sum(rows) as rows
FROM system.parts
WHERE database = 'nostr' AND active
GROUP BY table;
```

---

## Backup & Recovery

### RocksDB Backup

The RocksDB dedupe index is rebuildable from the archive, but backing it up saves rebuild time:

```bash
# Stop services first
sudo systemctl stop pensieve

# Create backup
tar -czf rocksdb-backup-$(date +%Y%m%d).tar.gz /data/rocksdb

# Restart
sudo systemctl start pensieve
```

### ClickHouse Backup

ClickHouse data is rebuildable from the archive. For faster recovery:

```bash
# Native ClickHouse backup (to Storage Box)
docker compose exec clickhouse clickhouse-client \
  --query "BACKUP DATABASE nostr TO Disk('backups', 'nostr-$(date +%Y%m%d).zip')"
```

### Full Rebuild from Archive

If you need to rebuild ClickHouse from scratch:

```bash
# 1. Reset ClickHouse
docker compose down -v  # WARNING: deletes all ClickHouse data
docker compose up -d clickhouse

# 2. Re-run all segments through the indexer
# (implementation depends on your indexer binary)
```

---

## Troubleshooting

### ClickHouse Won't Start

```bash
# Check logs
docker compose logs clickhouse

# Common issues:
# - Disk full: check df -h /data
# - Permission denied: check chown -R 101:101 /data/clickhouse
# - Corrupt data: may need to remove /data/clickhouse and rebuild
```

### Archive Sync Failing

```bash
# Test rclone connection
rclone ls storagebox:

# Check timer status
systemctl status archive-sync.timer

# Run sync manually with verbose output
rclone sync /archive/segments storagebox:pensieve/archive -v
```

### High Memory Usage

ClickHouse will use available RAM for caching. This is normal. To limit:

```xml
<!-- clickhouse/config.xml -->
<max_server_memory_usage_to_ram_ratio>0.7</max_server_memory_usage_to_ram_ratio>
```

