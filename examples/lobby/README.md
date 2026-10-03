# Lobby örneği (B21)

Uçtan uca imzalı bilet akışı, tek süreçte:

```text
istemci ──giriş──▶ lobi ──izin {kapılar, rUDP anahtarı, bilet, oda}──▶ istemci
istemci ──rUDP el sıkışma (sunucu anahtarı izinden sabitlenir)──▶ oyun sunucusu
istemci ──AUTH {bilet}──▶ doğrulayıcı: imza, standart talepler, oyunun kontrolü
istemci ──JOIN──▶ oyun karakteri DOĞRULANMIŞ taleplerden yerleştirir
```

- `src/lobby.rs` — lobi: hesaplar, karakterler, imzalı izin (HTTP uç
  noktasının yerine düz bir fonksiyon; gerçek lobi oyunun kendi servisi).
- `src/server.rs` — oyun sunucusu: mühürlü rUDP + TCP kapısı, bilet
  kancası (`Validator<Loadout>` + oyunun kontrolü).
- `src/game.rs` — oyun: 2B demo; her karakter sınıfının tarafında doğar
  (büyücü batıda, savaşçı doğuda) — sınıf istemcinin sözünden değil,
  biletin imzalı `ext`'inden okunur. Bilinmeyen sınıfı oyunun kontrolü
  `unknown_class` adıyla reddeder.
- `src/client.rs` — istemci: izinle bağlan, katıl, kendi karakterini
  anlık görüntüde gör.

Çalıştırma:

```sh
cargo run -p gsb-example-lobby
```

Beklenen çıktı: `ann` ve `bob` girer (x = -30 / +30), `eve` oyunun
kontrolüne takılır (`unknown_class`), sahte imzalı bilet (`signature`) ve
süresi dolmuş bilet (`expired`) reddedilir — hepsi ERROR 10, bağlantı
açık kalır. Ayrıntılı log için `RUST_LOG=warn`.

Testler (`tests/lobby.rs`) aynı akışı ve sayaçları kilitler; ikinci test
sunucuyu `[ticket]` tablosundan kurar. Belge: `docs/TICKETS.md`.
