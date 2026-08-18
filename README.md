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
- **Taşıma:** varsayılan TCP; `Transport`/`Listener`/`Endpoint` soyutlaması
  sayesinde yarın rUDP eklenir, aktör koduna tek satır dokunulmaz.

## Crate haritası

| Crate | Görev |
|---|---|
| `gsb-lint` | Build-helper: yasak desenleri (`tokio::select`, `Mutex`, …) taraması |
| `gsb-protocol` | Kabuk/opcode/`MessageTable` + temel protobuf mesajları |
| `gsb-ecs` | `System` trait'i, `SystemRunner` |
| `gsb-core` | ID'ler, kanallar, global ticker, registry actor, oda actor (5 fazlı tick), bağlantı actor |
| `gsb-net` | `Transport`/`Listener`/`Endpoint` + pump görevleri + varsayılan TCP |
| `gsb-game` | **Tüm oyun mantığı** (component'ler, sistemler, `RoomLogic`, oyun protosu) |
| `gsb-server` | Compozisyon kökü: config, başlatma, `gsb-server` binary'si + client örneği |

## Hızlı başlangıç

```sh
cargo test --workspace          # 33 test: framing, lint, ticker/oda tick'i, kare-bağımsızlık, kimlik değişmezi, e2e
cargo run -p gsb-server         # varsayılan config (0.0.0.0:7777, 1 oda, 30 Hz global)
cargo run -p gsb-server -- config.example.toml
cargo run -p gsb-server --example client   # AUTH + JOIN + MOVE_TO, snapshotları yazdırır
```

`config.example.toml` dosyasındaki tüm anahtarlar isteğe bağlıdır; eksik olanlar
için gömülü varsayılanlar kullanılır.

## Çevrimiçi protokol

```text
[u32 LE gövde uzunluğu][u16 LE opcode][protobuf payload]
```

Uzunluk öneki **sadece taşıma katmanında** yaşar (çerçevelemeyi transport
sahiplenir). Opcode bantları: `1..=64` temel kontrol (auth/join/leave/heartbeat/
error), `1000+` oyun bandı (`MOVE_TO=1000`, `WORLD_SNAPSHOT=1003`,
`PRIVATE=1004`).

Bant genişliği: yayın, entity başına **tam, kendi kendine yeten snapshot**
gönderir; yavaş istemciye düşen batch en fazla 1 tick bayatlık yaratır.
Delta + AOI, belgelenmiş sonraki adımdır (`docs/DESIGN.md`).

## Bir sonraki oyun

Yeni oyun mantığı yazmak = sadece `gsb-game` crate'ini değiştirmek:

1. `proto/game.proto` + op'leri güncelle;
2. component/`System` tanımla;
3. `RoomLogic<World>` implemente et;
4. `gsb-server` içindeki `demo_room_factory()` ve `build_table()` çağrısını
   yeni factory'ye bağla.

Core/net/protocol/ecs crate'lerine dokunulmaz.

## Belgeler

- `docs/DESIGN.md` — mimari kararlar, ölçekleme, kısıtlar ve yol haritası.
