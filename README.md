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
- **Her oda 2 görev:** pacer (sabit hızda `Tick` gönderir) + room actor
  (dünyanın tek sahibi; 4 fazlı tick).
- **Oda tick'i 4 faz:** READ (aksiyonları boşalt) → CONVERT (aksiyon →
  component) → SYSTEMS (oyun sistemleri) → BROADCAST (dirty entity →
  bağlantı başına tek batch).
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
| `gsb-ecs` | `System` trait'i, `SystemRunner`, `EntityVersion` dirty tracking |
| `gsb-core` | ID'ler, kanallar, registry actor, oda actor (4 fazlı tick), bağlantı actor |
| `gsb-net` | `Transport`/`Listener`/`Endpoint` + pump görevleri + varsayılan TCP |
| `gsb-game` | **Tüm oyun mantığı** (component'ler, sistemler, `RoomLogic`, oyun protosu) |
| `gsb-server` | Compozisyon kökü: config, başlatma, `gsb-server` binary'si + client örneği |

## Hızlı başlangıç

```sh
cargo test --workspace          # 14 test: framing, lint, oda tick'i, e2e
cargo run -p gsb-server         # varsayılan config (0.0.0.0:7777, 1 oda, 30 Hz)
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
error), `1000+` oyun bandı (`MOVE_TO=1000`, `ENTITY_SPAWNED=1001`,
`ENTITY_REMOVED=1002`, `ENTITY_STATE=1003`).

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
