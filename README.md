# gsb — game-server-base

MOBA / MMORPG projeleri için **kilit'siz, select'siz, saf kanal tabanlı aktör**
mimarisine sahip bir Rust oyun sunucusu temeli.

> Hedef: 100k+ eşzamanlı bağlantıya ölçeklenebilir, taşıma katmanı (transport)
> değiştirilebilir, oyun mantığı tek crate'de izole edilmiş bir temel.

## Mimari özet

- **Tüm aktörler sadece mailbox'a `recv()` eder.** Ne `tokio::select!`, ne
  kilit (`Mutex`/`RwLock`), ne park eden bekleme — yok. Bu kural **derleme
  zamanında** `gsb-lint` build-helper crate'i tarafından zorlanır.
- **Her bağlantı 3 görev:** reader pump + connection actor + writer pump.
- **Tek global ticker** (tek görev, `broadcast` kanal) + **her oda 1 görev:**
  room actor (dünyanın tek sahibi). Oda actor'ünün *tek* await'i global
  tick `recv()`'i; tick gövdesi tamamen senkron. Oda hizi global hızın
  tam bölünür olmalı (60 Hz global / oda 15 Hz → her 4. tick'te adım).
- **Oda tick'i 5 faz:** CONTROL (join/leave/shutdown çek) → READ
  (bağlantı başına aksiyon kanallarını `try_recv`) → CONVERT (aksiyon →
  component) → SYSTEMS (oyun sistemleri) → BROADCAST (grup başına tam
  dünya snapshot'ı, bir kez kodlanır, üyelerle referansla paylaşılır;
  bağlantı başına tek batch + flush).
- **Girdi izolasyonu:** her bağlantının kendi `Action` kanalı var;
  connection actor gelen oyun op'larını `try_send` ile odaya iletir —
  kanal doluysa girdi atılır (o oyuncunun girdisi, o oyuncunun izolasyonu).
- **Kare hızından bağımsızlık:** simülasyon `dt = gerçek geçen süre` ile
  ilerler; kaçırılan tick'ler tek catch-up adımıyla telafi edilir
  (üst sınır: 4 periyot). 15 Hz'de de 100 Hz'de de aynı gerçek sürede
  aynı mesafe (`gsb-game/tests/frame_independence.rs`).
- **ECS:** `bevy_ecs` (standalone). Oda actor'ü bir `World`'i münhasıran
  kendisi tutar; core, world tipine `W` jeneriği ile tamamen ECS'sizdir.
- **Protokol:** protobuf (`prost` / Unity'de `Google.Protobuf`).
  `[u32 LE uzunluk][u16 LE opcode][protobuf payload]`.
- **Taşıma:** aktör koduna tek satır dokunmayan `Transport`/`Listener`/
  `Endpoint` soyutlamasının arkasında beş kapı: düz TCP (varsayılan),
  TLS-TCP (rustls), rUDP (`transport = "udp"`; stateless cookie el
  sıkışması — cookie 10 sn'lik dilimlerle döner, yakalanan proof
  10-20 sn içinde geçersizleşir; kontrol bandı güvenilir `REL` — ACK
  ilerlemesi 5 sn boyunca hiç olmazsa bant ölü ilan edilir ve OTURUM
  BİTER, sessiz vazgeçme yok; oyun bandı kayıp-toleranslı `RAW`),
  QUIC (quinn; tek bi-stream + length-prefix = TCP semantiği) ve
  WebSocket (RFC 6455; her binary mesaj bir game frame). Aynı odalara
  hizmet veren karışık kapı listesi `[[listeners]]` tablosuyla kurulur.

## Crate haritası

| Crate | Görev |
|---|---|
| `gsb-lint` | Build-helper: yasak desenleri (`tokio::select`, `Mutex`, …) taraması |
| `gsb-protocol` | Kabuk/opcode/`MessageTable` + temel protobuf mesajları |
| `gsb-ecs` | `System` trait'i, `SystemRunner` |
| `gsb-core` | ID'ler, kanallar, global ticker, registry actor, oda actor (5 fazlı tick), bağlantı actor |
| `gsb-net` | `Transport`/`Listener`/`Endpoint` + pump görevleri + varsayılan TCP |
| `gsb-game` | **Tüm oyun mantığı** (component'ler, sistemler, `RoomLogic`, oyun protosu) |
| `gsb-server` | Compozisyon kökü: config, başlatma, `gsb-server` binary'si + client örneği + `gsb-loadgen` yük üreticisi |

## Hızlı başlangıç

Gereksinimler: Rust **1.95.0** (`rust-toolchain.toml` ile sabit; MSRV de
bu) — **başkası yok**. Sistemde `protoc` kurulu olmak zorunda değil:
proto build script'leri (`gsb-protocol`, `gsb-game`)
`protoc-bin-vendored`'ın gömülü ikilisini `prost-build`'e açıkça verir
(`Config::protoc_executable`), bu da `PROTOC`/`PATH` aramasının önüne
geçer. Gömülü ikilinin bulunmadığı egzotik bir hedefte build script bir
`cargo:warning` basıp eski davranışa (`PROTOC`, sonra `PATH`) düşer;
orada sistem `protoc`'u gerekir. CI: `.github/workflows/ci.yml` (fmt ·
clippy `-D warnings` · test).

```sh
cargo test --workspace          # 356 test: framing, lint, ticker/oda tick'i, RPC (tek oda + shard — rpc_shard süiti), bilet/kontrol düzlemi, READ adaleti (döner imleç), supervision (panik eden oda/shard), tablo budama (epoch/tombstone TTL, metrik emekliliği), reconnect (detach/resume/bot devri, PlayerId sürekliliği), trait birleşimi (GameLogic + sharded keepalive + shard-RPC), güvenlik (TLS taşıması, auth rate-limit, pre-auth cap'ler), çoklu-listener (aynı haritada karışık transport: TCP/TLS/rUDP/QUIC/WS), border-delta exchange, ops yüzeyi (/metrics, /healthz, admin API), görünürlük (delta AOI, PVS, takım sisi, sharded), kare-bağımsızlık, kimlik değişmezi, yayınlanabilirlik, e2e, metrik akışı, yük dumanı, wire sözleşmesi (RPC zarfı baytları, emekli opcode'lar), ERROR kod numaralandırması, protokol sürümü el sıkışması, oturum yaşam döngüsü (idle penceresi + yazma tıkanması) ve heartbeat-ACK kısması, AFK sinyali (girdi-boşta saati + varsayılan kapalı tavan)
cargo run -p gsb-server         # varsayılan config (0.0.0.0:7777, 1 oda, 30 Hz global)
cargo run -p gsb-server -- config.example.toml
cargo run -p gsb-server --example client   # AUTH + JOIN + MOVE_TO, snapshotları yazdırır
RUST_LOG=info cargo run -p gsb-server   # ayrıca saniyelik gsb-metric metrik satırları (DESIGN §12)
```

`config.example.toml` dosyasındaki tüm anahtarlar isteğe bağlıdır; eksik olanlar
için gömülü varsayılanlar kullanılır.

## Yük testi (gsb-loadgen)

Kalıcı yük üreticisi binary'si (ilk uçtan uca sayılar ve doyma analizi:
`docs/CHANGELOG.md` "metrik + yük turu"):

```sh
cargo run -p gsb-server --bin gsb-loadgen -- 500 --duration 10
# N istemci (vars. 100), 10 s pencere, 150 ms MOVE_TO, tek oda.
# In-process: gerçek sunucu (ephemeral port) + N gerçek TCP istemcisi;
# sunucu tarafı metrikler kanaldan yakalanır (stdout parse yok).
# --stagger-ms MS: istemci i, i×MS gecikmeyle bağlanır (vars. 0 = hepsi birden;
#  loopback burst'i accept yolunun en kötü halidir — bkz. ROADMAP bulgusu).
# --addr HOST:PORT: harici sunucuya yalnız istemci modu.
```

Çıktı: insan-okunur rapor + tek satır `RESULT mode=.. clients=.. joined=..
snap_per_client_p50=.. tick_hz_med=.. server_hz=.. step_p50_us=.. dropped=..
server_in_bps=.. server_out_bps=.. peak_conns=..` (scriptlenebilir).
Duman testi (`gsb-server/tests/loadgen_smoke.rs`) suite'in içinde binary'yi
gerçekten spawn eder ve `RESULT`'ı doğrular; ağırlıklı koşular bilinçli olarak
suite dışında (yavaşlatmasın, flaky yapmasın).

## Çevrimiçi protokol

```text
[u32 LE gövde uzunluğu][u16 LE opcode][protobuf payload]
```

Uzunluk öneki **sadece taşıma katmanında** yaşar (çerçevelemeyi transport
sahiplenir). Opcode bantları: `1..=64` temel kontrol (auth/join/leave/heartbeat/
error), `1000+` oyun bandı (`MOVE_TO=1000`, `WORLD_SNAPSHOT=1003`,
`PRIVATE=1004`).

**Sürüm ve hata sözleşmesi** (protokol sertleştirme turu, DESIGN §5.2-5.5):

- İstemci ilk frame'inde (`AUTH_REQ`) `Auth.protocol_version` gönderir;
  `gsb_protocol::PROTOCOL_VERSION` tek doğruluk kaynağıdır. `0` =
  sürümsüz (eski istemci) — kabul edilir, loglanır. Uyuşmazlık: `ERROR`
  kod 13, **bağlantı yaşar** (istemci yeniden derlenmeli). Tek kontrol,
  ek round trip yok.
- `ERROR` kodları bir yorum tablosu değil, üretilen bir proto enum'ı:
  `gsb.base.ErrorCode`. Bilmediği bir kodu alan istemci onu
  `ERROR_CODE_OTHER` gibi işler (ham sayı korunur, loglanabilir); `0`
  asla gönderilmez.
- RPC zarfının **iki yarısı da** `base.proto`'da (`RpcRequest` +
  `RpcResponse`); `game.proto` onu `import` eder.
- Kaldırılmış alan numaraları `reserved` (bkz. `EntityRecord`), emekli
  opcode'lar `gsb_game::op::RETIRED` listesinde ve testle kilitli.

Bant genişliği: yayın, entity başına **tam, kendi kendine yeten snapshot**
gönderir; yavaş istemciye düşen batch en fazla 1 tick bayatlık yaratır.
Delta kodlanmış AOI snapshot'ları (spatial strateji), takım sisi (fog of
war), PVS ve sharded odalar bugün implemente edilmiş durumda; hepsi config
üzerinden `visibility` anahtarıyla seçilir (`all` / `spatial` / `team` /
`pvs` / `sharded`).

## Bir sonraki oyun

Yeni oyun mantığı yazmak = sadece `gsb-game` crate'ini değiştirmek:

1. `proto/game.proto` + op'leri güncelle;
2. component/`System` tanımla;
3. `RoomLogic<World>` implemente et;
4. `gsb-server` içindeki `demo_room_factory()` ve `build_table()` çağrısını
   yeni factory'ye bağla.

Core/net/protocol/ecs crate'lerine dokunulmaz.

## Belgeler

- `docs/DESIGN.md` — mimari kararlar, ölçekleme, metrik altyapısı (§12),
  kısıtlar ve yol haritası.
- `docs/ROADMAP.md` — açık işler ve öncelikler.
- `docs/CHANGELOG.md` — tamamlanan turların tam kaydı (ham yük testi
  sayıları dahil).
- `docs/TICK-ARCHITECTURE.md` — broadcast tabanlı tick mimarisi.
- `docs/RPC-CONTROL-PLANE.md` — RPC deseni ve kontrol düzlemi tasarımı.
- `docs/OPS.md` — ops yüzeyi tasarımı (/metrics, /healthz, admin API).
- `docs/TRAIT-ARCHITECTURE.md` — GameLogic birleşimi, PlayerId yolu.
- `docs/RECONNECT.md` — kopan oyuncu politikası (detach/resume/bot devri)
  tasarımı — **uygulandı** (ROADMAP P1 `[x]`; core mekaniği + demo park
  politikası, `crates/gsb-core/tests/reconnect.rs`), uygulamanın
  sözleşmesi olarak geçerliliğini koruyor.
- `CONTRIBUTING.md` — katkı disiplini (lint yasakları, aktör kuralları,
  fmt/clippy/test kapıları).

## Lisans

MIT — `LICENSE`.
