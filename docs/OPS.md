# gsb: Ops Yüzeyi Tasarımı — /metrics, /healthz, Admin API

> Durum: TASARIM (uygulama bu dokümanı sözleşme alır). Reconnect sonrası
> sunucu uzun ömürlü durum tutuyor (park edilmiş oyuncu, pending RPC,
> oturum sayaçları) — gözlemlenemeyen durum kör noktadır; bu doküman
> mevcut metrik/kontrol hattını HTTP'ye açar.

## 1. Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | HTTP sunucusu **elle yazılmış minimal HTTP/1.1** (tokio `TcpListener` üstünde) | Sıfır yeni dependency; Prometheus text formatı ve 3 endpoint için framework ağır gelir. Projenin kendi geleneği (rUDP, framing, lint hep elle). Elenenler: axum/hyper — bağımlılık bütçesi + yüzey alanı. Yalnız `GET` + `Connection: close`; keep-alive/chunking yok (NOT-DONE) |
| 2 | Metrik akışı **`tokio::sync::watch`** ile | `MetricSink` enum'üne üçüncü varyant: `Watch(watch::Sender<MetricReport>)`. Collector her rapor periyodunda en son raporu watch'a iter; HTTP task `borrow()` ile okur. Kilit yok (tek yazar, tek güncel değer — tam watch semantiği); lint'e takılan paylaşımlı-state deseni de girmez |
| 3 | **JSON yok** — admin parametreleri query-string/form-encoded | Elle JSON parser'ı yazmak red; admin yüzeyi 3 operasyon, JSON gerektirmiyor. Elenen: serde_json bağımlılığı |
| 4 | Varsayılan bağlama **127.0.0.1**, config ile açılır/kapatılır | `http_listen = ""` → devre dışı (varsayılan davranış değişmesin); adres verilirse dinler. v1'de auth yoktur (NOT-DONE): localhost dışına açmak operatörün bilinçli kararıdır ve dokümanda uyarılır |
| 5 | Prometheus **text exposition formatı** (`# HELP/# TYPE` + örnek satırları) | Standart scrape formatı; `MetricReport` alanlarından üreten ayrı render fonksiyonu (unit-test edilebilir, HTTP'den bağımsız) |

## 2. Endpoint'ler

| Yöntem | Yol | İşlev |
|---|---|---|
| GET | `/healthz` | `200 ok` — süreç ayakta + ticker yaşıyor mu (son rapor yaşı eşiği aşmadıysa ok; aşıysa 503, reason body'de) |
| GET | `/metrics` | Prometheus text — tüm RegistrySample/RoomSample/ConnSample sayaçları (`gsb_*` önekli) |
| GET | `/rooms` | Tablodaki odalar + durumları (id, members, persistent) — kontrol düzleminin table-only cevabı |
| POST | `/rooms/open?id=&tick_hz=` | Runtime oda açma (`ServerHandle.open_room` idempotent-create sözleşmesiyle); oda sunucunun o id'li odasıdır — varsa `[rooms.<id>]` dahil (aşağıda) |
| POST | `/rooms/close?id=` | Oda kapatma (`close_room`; persistent ise emeklilik semantiği işler) |

Admin yolları mevcut `ServerHandle` komutlarını kullanır — yeni bir kontrol yolu AÇILMAZ, yalnız transport eklenir.

**`/rooms/open`'ın açtığı oda = sunucunun odası (BACKLOG F8).** Admin
yüzeyi kendi oda varsayılanını UYDURMAZ: açılan odanın `RoomConfig`'i,
başlangıçta ön-kurulan odaların geldiği şablondur
(`config/axes/listeners/room.rs`: `Config::room_template` → tek eşleme;
`Config::room_config(id)` onun üstünde). Oda düzeyindeki bütün anahtarlar
— `tick_hz`, `room_control`, `conn_action`, `max_snapshot_bytes`,
`keepalive_hz`, `max_players`, `max_idle_input_secs`, `afk_action`,
`max_detach_hold_secs`, `input_rate_hz`, `input_burst` — runtime odaya
da gider; oyunun `[game]`
tablosu zaten gidiyordu (fabrika, `spawn_registry`'de bir kez kurulur ve
registry her `CreateRoom`'da — başlangıç ya da admin — aynı fabrikayı
çağırır). Eskiden yüzey `RoomConfig::default()` + `tick_hz` kuruyordu:
aynı sunucunun runtime odası başka bir tavanla, başka kapasitelerle
çalışıyordu.

- **İstek başına tek geçersiz kılma `tick_hz`** (verilmezse odanın
  hızı: id'nin `[rooms.<id>]`'inde yazılıysa o, değilse sunucununki;
  doğrulama aynı: sonlu pozitif sayı değilse 400, registry'ye gitmez;
  global hızı bölmüyorsa registry'nin 400'ü). HTTP'ye başka bir
  geçersiz kılma EKLENMEDİ: oda anahtarları operatörün politikasıdır
  (kapasite, `max_players`, `max_detach_hold` gibi güvenlik tavanları)
  ve kimliksiz v1 yüzeyinin (karar 4) bir güvenlik tavanını oda başına
  gevşetebilmesi istenmez; her ek parametre idempotent karşılaştırmaya
  bir çatışma ekseni daha ekler; query grameri bilerek küçük (karar 3).
  `tick_hz` farklı: odanın hız sınıfı gerçek bir oda özelliği (global
  hızı bölen daha yavaş oda). Oda başına farklı ayar config dosyasında
  verilir (aşağıda, B18) — dosya operatörün, yüzey kimliksiz; programatik
  yol da açık: `ServerHandle::open_room(RoomConfig { …,
  ..cfg.room_config(id) })`.

**Oda başına override: `[rooms.<id>]` (BACKLOG B18).** Bir oda (yüksek
yoğunluklu bir oda, bir lobi) sunucunun oda anahtarlarından kendi
değerlerini alabilir:

```toml
tick_hz = 30.0
room_count = 3
max_players = 200

[rooms.2]          # lobi: kalabalık, yavaş
max_players = 2000
tick_hz = 10       # 30'u böler (her 3. global tick)

[rooms.9]          # room_count'un ötesi: runtime'da açılınca geçerli
max_players = 16
max_detach_hold_secs = "off"
```

- **Anahtarlar:** yalnız oda düzeyindekiler — `tick_hz`, `room_control`,
  `conn_action`, `max_snapshot_bytes`, `keepalive_hz`, `max_players`,
  `max_idle_input_secs`, `afk_action`, `max_detach_hold_secs`,
  `input_rate_hz`, `input_burst`; yazım ve anlam düz
  anahtarlarınki (`max_players = 0` sınırsız, `max_idle_input_secs = 0`
  kapalı, `max_detach_hold_secs` üç yazımıyla). Yazılmayan anahtar
  sunucunun değerini korur. Nokta yazımı da aynı şey:
  `rooms.2.max_players = 2000`.
- **Katmanlama tek yerde:** `Config::room_template` şablonu ve
  override'ları birlikte taşır (`RoomTemplate`); `RoomTemplate::room(id)`
  sunucunun odasını kurar ve id'nin override'ını üstüne koyar. Başlangıç
  odaları, `/rooms/open` ve `Config::room_config(id)` hepsi oradan kurar
  — bir yolun override'ı atlaması yapısal olarak mümkün değil.
- **Öncelik (düşükten yükseğe):** çekirdeğin varsayılanı → düz oda
  anahtarları → `[rooms.<id>]` → `/rooms/open`'ın `tick_hz` query'si. İstek
  en özgül söz: operatör bir odayı bilerek başka hızda açıyorsa dosyanın
  hızı onu ezmez; çatışma zaten idempotentlik kuralıyla yakalanır (canlı
  oda başka hızdaysa 409). *Elenen:* override'ın query'yi ezmesi — açıkça
  yazılmış bir istek parametresini sessizce yok saymak ya da ayrı bir
  4xx gerektirirdi.
- **İdempotentlik:** registry ortaya çıkan config'i karşılaştırır. Override'lı
  bir başlangıç odasını query'siz (ya da override'ın hızıyla) yeniden
  açmak **200**; sunucunun hızıyla açmak artık **409** (o oda o hızda
  değil). Override'lı runtime id'yi tekrar açmak 200.
- **Doğrulama (başlatmada, bir şey bağlanmadan):** bilinmeyen anahtar ya
  da oda düzeyi olmayan anahtar (`bind`, `max_connections`, bir oyunun
  `teams`'i…) ayrıştırma hatası — hata anahtarı adlandırır ve bölümün
  aldığı anahtarları sayar; id pozitif ve düz yazılmış bir tam sayı
  olmalı (`[rooms.0]`, `[rooms.07]`, `[rooms.lobby]` hata; TOML aynı
  anahtarı iki kez zaten kabul etmez). Registry'nin reddedeceği oda —
  global hızı bölmeyen `tick_hz`, odanın `tick_hz`'inden büyük
  `keepalive_hz` — `ServerError::RoomOverride { id, source }` ile
  başlatmayı durdurur; kural çekirdeğin kendisi
  (`RoomConfig::step_divisor`, registry'nin create'i de onu çağırır),
  kopyası değil. Override olmasa başlangıç odası bu durumda yalnız
  uyarı loglardı (düz anahtarların bu davranışı değişmedi).
- **`conn_action = 0` başlatmayı durdurur (F21), düz ya da
  `[rooms.<id>]` içinde:** `ServerError::RoomKey` anahtarı ve yazıldığı
  yeri adlandırır (`` `[rooms.7]` `conn_action` = 0: … ``). Eskiden değer
  `mpsc::channel(0)`'a ulaşıp oda aktörünü ilk join'de panikletiyordu
  (`room_control` `gsb_core::channel::channel`'ın `max(1)`'i ile
  korunuyordu). Sıfır kapasiteli bir aksiyon kanalının anlamı yok:
  çekirdek de `0`'ı artık tek yuva okur (`RoomConfig::action_channel`,
  odanın join/resume'u ve shard'ın ikisi tek kurucudan geçer) — elle
  kurulmuş bir config (kütüphane kullanımı) odayı panikletemez; dosyada
  `0` yazan operatör ise başka bir şey kastetmiştir ve bunu başlatmada
  duyar. *Elenen:* yalnız kıstırmak — `0` yazan operatörün niyeti (ör.
  "sınırsız", başka kapların `0`'ı gibi) sessizce "1"e dönerdi.
- **Girdi hız sınırı: `input_rate_hz` / `input_burst` (BACKLOG E1,
  SECURITY §3.4).** Bağlantı başına token bucket — saniyede
  `input_rate_hz` aksiyon, bir anda en çok `input_burst` (yazılmazsa bir
  saniyelik: `input_rate_hz`). Düz yazılırsa her odanın, `[rooms.<id>]`
  içinde yazılırsa yalnız o odanın `RoomConfig::input_rate`'i. Katmanlar
  (düşükten yükseğe): çekirdek (KAPALI) → oyunun sayısı
  (`GameModule::input_rate`, varsayılan yok) → düz anahtarlar →
  `[rooms.<id>]`. `input_rate_hz = 0` KAPALI demek, oyunun sayısının da
  üstünde. İkisi tek limit olarak okunur: bir tablo limit yazıyorsa
  tamamını yazar — `input_burst` aynı tabloda `input_rate_hz > 0`
  olmadan (yalnız, `input_rate_hz = 0`'ın yanında) ya da `0` ise
  `ServerError::RoomKey` ile başlatma durur (bir katmanın hızıyla
  diğerinin burst'ü karışmaz). Oyunun sayısı dosyada görünmediği için
  `Config::room_config(id)` dosyanın görünümüdür; çalışan sunucunun
  kurduğu oda — başlangıç, `/rooms/open` ve `ServerHandle::room_config(id)`
  tek şablondan — oyunun sayısını taşır; ön-kurulan bir odayı
  `handle.open_room(handle.room_config(id))` ile yeniden açmak
  idempotent kalır.
- **Girdi-boşta tavanının eylemi: `afk_action` (BACKLOG E6,
  RECONNECT §16.1).** `"leave_room"` ya da `"disconnect"`; başka her
  yazım (`"kick"`, büyük harf, tire, sayı) başlatmayı iki yazımı adlayarak
  durdurur. Düz yazılırsa her odanın, `[rooms.<id>]` içinde yalnız o
  odanın `RoomConfig::afk_action`'ı. Katmanlar (düşükten yükseğe):
  çekirdek (`leave_room`) → oyunun varsayılanı (`GameModule::afk_action`,
  varsayılan `LeaveRoom`) → düz anahtar → `[rooms.<id>]`. İkisi de önce
  üyeyi oyunun `on_disconnect`'ine verir (park / AI devri / despawn):
  `leave_room` orada durur — soket açık, tel sessiz; bağlantı odada
  değildir (kendi `LEAVE_ROOM_REQ`'inden sonraki gibi): oyun kareleri
  `ERROR 6` alır, istemci doğrudan yeniden katılır, park edilmiş varlığını
  geri alır (B40, RECONNECT §16); `disconnect` bağlantıyı da kapatır —
  istemciye en-iyi-çaba ERROR 9 (`input idle: no game input for N s
  (…)`), sonra kapanış; `server_closes{reason="idle_input"}` sayılır;
  park edilmiş varlık aynı kimlikle yeniden bağlanınca geri alınır.
  Tavan (`max_idle_input_secs`) yoksa etkisizdir — başlatmayı DURDURMAZ:
  düz `disconnect`, tavanı yalnız `[rooms.<id>]`'de olan bir odaya da
  hizmet eder. Oyunun varsayılanı dosyada görünmez
  (`Config::room_config` dosyanın görünümü, `ServerHandle::room_config`
  çalışan sunucununki — `input_rate` gibi).
- **`room_count`'un ötesindeki id hata DEĞİL:** runtime odaları
  herhangi bir pozitif id ile açılır; `[rooms.9]` tam da `/rooms/open?id=9`'un
  açacağı odayı tanımlar. Hata yapmak bu kullanımı yasaklardı; uyarı
  doğru bir config'te gürültü olurdu. Bunun yerine başlatmada bir `info`
  satırı ("boot odası değil, runtime'da açılınca geçerli") yazım hatası
  bir id'yi görünür kılar.
- **Varsayılan aynı:** `[rooms]` yoksa her oda bugünkü gibi sunucunun
  odası (F8'in alan alan testiyle kilitli). İstemci teli değişmedi.
- *Elenen şekil:* `[[rooms]]` + `id = 7` (listeners gibi dizi) — aynı id
  iki kez yazılabilir, ayrı bir tekrar kontrolü gerekir ve `id` unutulabilir;
  odaların doğal bir anahtarı var, TOML tablosu tekrarı yapısal olarak
  engeller. *Elenen:* override'ı oyuna ham tablo olarak vermek — oda
  anahtarları motorun, oyunun değil (GAME-MODULE §4.3).
- **Sözleşme değişikliği (düzeltmenin sonucu):** durum kodları, hata
  gövdeleri ve idempotentlik kuralı aynı. Değişen yalnız istenen
  config'in kendisi, iki görünür sonucu var: (1) oda anahtarları
  varsayılandan farklı bir sunucuda ön-kurulan bir odayı aynı hızla
  yeniden açmak artık idempotent **200** (eskiden 409 — istek
  `default()` idi, oda sunucunun config'iyle kurulmuştu); (2)
  `keepalive_hz` artık sunucununki, bu yüzden ondan küçük bir `tick_hz`
  isteği registry'nin `KeepaliveRate`'iyle **400** alır (eskiden
  varsayılan 1 Hz'e göre karar verilirdi) — ön-kurulan odalarla aynı
  kural.

**Sunucu düzeyi: dinleme kuyruğu `listen_backlog` (BACKLOG B84).**
Oda değil soket anahtarı: TCP tabanlı her dinleyen soketin accept
kuyruğu — TCP, TLS ve WS kapıları (düz anahtarlardan türeyen tek kapı
ya da `[[listeners]]`'ın her girdisi) ve bu yüzeyin kendi soketi
(`http_listen`). UDP kapıları (rUDP, QUIC) kuyruksuz; anahtarı görmez.

```toml
listen_backlog = 4096   # vars. 128; çekirdek somaxconn'da keser
```

- **Varsayılan 128 = bugünkü kuyruk:** tokio'nun kendi bind'i
  (`TcpListener::bind` → mio ≥ 1.1 → std'nin değeri) `listen(2)`'ye 128
  veriyordu; anahtar yazılmazsa her soket aynı kuyruğu alır. Değer
  artık `gsb_net::listen::DEFAULT_LISTEN_BACKLOG`'da: bir bağımlılık
  yükseltmesi onu sessizce değiştiremez.
- **Çekirdek tavanı:** soketin aldığı kuyruk `min(listen_backlog,
  somaxconn)` (Linux `net.core.somaxconn`, 5.4'ten beri vars. 4096;
  macOS `kern.ipc.somaxconn`, vars. 128). Tavanı aşan değer hata değil,
  kesilir; daha büyük kuyruk için önce sysctl.
- **Doğrulama (başlatmada, bir şey bağlanmadan):** `1..=2147483647`
  (`listen(2)` C `int` alır); `0` ya da fazlası
  `ServerError::BadListenBacklog`. Negatif değer ayrıştırma hatası.
- **Katman yok:** tek sunucu anahtarı. `[rooms.<id>]` de bir
  `[[listeners]]` girdisi de onu bilinmeyen anahtar olarak reddeder
  (girdininki F61'den beri; aşağıda — önceden girdide sessizce
  etkisizdi). Kapı başına değer ölçülmüş bir ihtiyaç yokken eklenmedi;
  gerekirse girdiye isteğe bağlı bir alan olarak geriye uyumlu eklenir.
- **Ne zaman büyütülür:** katılma patlaması kuyruğu taşırdığında —
  Linux'ta `nstat -az TcpExtListenOverflows` (ya da
  `/proc/net/netstat`'ın `ListenOverflows`'u) patlama boyunca artar,
  istemcilerin bağlanma süresi ~1 sn'ye (SYN yeniden gönderimi) sıçrar.
  Beklenen patlamanın boyuna göre boyutlanır (SECURITY §4.4); ölçüm:
  RPC-CONTROL-PLANE §8.2 "B84".

**Sunucu düzeyi: UDP kapılarının soket arabellekleri
`udp_recv_buffer_bytes` / `udp_send_buffer_bytes` (BACKLOG B4).**
UDP tabanlı her dinleyen soketin çekirdek arabellekleri — rUDP (`udp`)
ve QUIC (`quic`) kapıları, düz anahtarlardan türeyen kapı ya da
`[[listeners]]`'ın her girdisi. UDP kapısının accept kuyruğu yok: TEK
soket kapının bütün oturumlarını taşır, patlama altında tek tampon
alma kuyruğudur. TCP tabanlı kapılar anahtarları görmez.

```toml
udp_recv_buffer_bytes = 4194304   # vars. yok = sistem varsayılanı
udp_send_buffer_bytes = 1048576   # vars. yok = sistem varsayılanı
```

- **Yazılmazsa dokunulmaz:** `setsockopt` çağrılmaz; soket sistem
  varsayılanını alır (Linux `net.core.rmem_default` / `wmem_default`,
  çoğu dağıtımda 212 992 B) — anahtarlardan önceki soketin aynısı.
- **Çekirdek kuralı (Linux):** istek `net.core.rmem_max` /
  `net.core.wmem_max`'ta kesilir (çoğu dağıtımda 212 992 — yani büyütmek
  çoğunlukla önce sysctl ister: `sysctl -w net.core.rmem_max=8388608`),
  sonra **ikiye katlanır** (ikinci yarı çekirdeğin muhasebesi):
  `getsockopt`/`ss -uamn` (`skmem` `rb`/`tb`) istenenin iki katını
  gösterir. Kapı bind'da verilen boyutları `info` log'lar
  (`UDP socket buffers`, `recv_buffer`/`send_buffer`); kesilen istek
  hata değil, sysctl'ü adlandıran bir `warn` (`capped by the kernel`).
- **Doğrulama (başlatmada, bir şey bağlanmadan):** `4096..=2147483647`
  (bir sayfadan C `int`'e); dışı `ServerError::BadUdpBuffer` (anahtarı
  ve değeri adlandırır). Negatif değer ayrıştırma hatası.
- **Katman yok:** tek sunucu anahtarı; `[rooms.<id>]` de `[[listeners]]`
  girdisi de reddeder (`listen_backlog` gibi).
- **Ne zaman büyütülür:** katılma patlamasında ya da girdi yelpazesinde
  (çok oturumun aynı tick'te yolladığı girdiler) çekirdek kuyruğu
  taşırdığında — Linux'ta `/proc/net/snmp`'nin `Udp:` satırındaki
  `RcvbufErrors` (ya da `nstat -az UdpRcvbufErrors`) patlama boyunca
  artar; rUDP'de el sıkışma yeniden gönderimleri (loadgen `hs_retries`)
  ve connect p99 büyür. Bu kayıp sunucunun hiçbir sayacında görünmez
  (demux onu hiç görmez); ölçüm: DESIGN §6 "UDP kapılarının soket
  arabellekleri".

**Sunucu düzeyi: kaynak başına el sıkışma sınırı
`max_handshakes_per_source` (BACKLOG D11).** El sıkışan her kapının —
WS, TLS, QUIC; düz anahtarlardan türeyen kapı ya da `[[listeners]]`'ın
her girdisi — bir kaynak adrese (IPv4 adresi, IPv6 /64) verdiği en çok
uçuştaki el sıkışma yuvası. Kapının kendi sınırı (`max_unauth_conns`,
SECURITY §4.3) değişmez; bu ondan tek kaynağın alabileceği pay. Düz TCP
(el sıkışma evresi yok) ve rUDP (durumsuz çerez) anahtarı görmez.

```toml
max_handshakes_per_source = 16   # vars. yok = sınır yok; 0 = yok
```

- **Varsayılan kapalı:** yazılmazsa (ya da `0`) kapılar bugünkü gibi.
  Açık varsayılan olmaz: aynı NAT adresinin arkasındaki oyuncular onu
  paylaşır, her test ve loadgen koşusu tek loopback adresinden bağlanır.
- **Ne sayılır:** uçuştaki el sıkışma (ham accept'ten accept döngüsünün
  uç noktayı almasına dek), bağlantı değil — biten el sıkışma sayıdan
  düşer. Dürüst bir el sıkışma bir-iki gidiş-dönüş sürer.
- **Sınır üstü:** WS/TLS'te soket el sıkışmasız kapanır; QUIC'te
  kanıtlanmış adres `refuse`, kanıtlanmamış adres durumsuz Retry alır
  (sahte kaynaklı Initial'larla kurbanın sayısını doldurmak kurbanı
  reddettiremez — SECURITY §4.3.1 #5). Sayaçlar §3 "Taşıma kapsamı:
  el sıkışan kapıların kaynak başına sınırı"; kaynak başına dönem
  başına tek `warn` kaynağı adlandırır.
- **Boyutlama:** aynı adresin arkasından aynı saniyede bağlanabilecek
  oyuncu sayısı + pay: ev/küçük ofis 8–16, LAN partisi ya da büyük
  CGNAT havuzunun arkasındaki bölge 32–64. `handshakes_refused_per_source`
  dürüst oyunculara düşüyorsa (saldırı yokken artıyorsa) büyütülür.
- **Katman yok:** tek sunucu anahtarı; `[rooms.<id>]` de `[[listeners]]`
  girdisi de reddeder. Negatif değer ayrıştırma hatası.

**Kapı girdisi: `[[listeners]]` bilinmeyen anahtarı reddeder (BACKLOG
F61 — 2026-09-28).** Bir girdi tam dört anahtar alır: `transport`,
`bind`, `tls_cert`, `tls_key`. Başka her anahtar — yazım hatası
(`tls_crt`, `bnd`) ya da kapıya yazılmış sunucu anahtarı
(`listen_backlog`, `max_connections`, `tick_hz`) — ayrıştırmayı, yani
başlatmayı bir şey bağlanmadan durdurur:

```text
gsb-server: cannot parse config file gsb.toml: TOML parse error at line 8, column 1
  |
8 | listen_backlog = 4096
  | ^^^^^^^^^^^^^^
unknown field `listen_backlog`, expected one of `transport`, `bind`, `tls_cert`, `tls_key`
```

- **Hata:** anahtarı adlandırır, girdinin aldığı anahtarları sayar ve
  satırı gösterir — girdi satırdan bulunur (girdi sırası ayrıca
  yazılmaz; `[rooms.<id>]` ve `[metrics]` ile aynı biçim, serde'nin
  `deny_unknown_fields`'ı + toml'un konumu). Eskiden anahtar sessizce
  atılıyordu: sunucu kalkıyor, değer hiçbir yere gitmiyordu. Yukarıdaki
  metin `gsb-server` ikilisinin stderr'e bastığı: `ConfigError`'ın
  `Display`'i, çıkış durumu 1 (F63'ten beri; aşağıda "Başlatma hatası").
- **Tasarım:** girdi düz bir struct — taşımaya özgü alt tablo,
  `flatten`'lı ya da etiketli (`tag`) bir parça yok (TLS dosyaları
  girdinin kendi iki alanı, WS/QUIC/rUDP düğmeleri sunucu düzeyinde).
  Bu yüzden serde'nin `deny_unknown_fields`'ı girdinin tamamını kapsar;
  serde'nin `flatten`/`tag` ile bu özniteliği birlikte düzgün
  işleyememesi burada söz konusu değil. Girdi ileride taşımaya özgü bir
  alt tablo alırsa her alt struct kendi `deny_unknown_fields`'ını
  taşımalı (ya da ham tablo üstünde bir doğrulama geçişi) — `flatten`
  değil.
- **Girdiler dosyanın SONUNA:** `[[listeners]]` bir TOML tablo dizisi;
  başlığından sonra yazılan her anahtar o girdiye aittir.
  `config.example.toml`'un iki kapılı örneği dosyanın ortasında, yorum
  olarak duruyor; yerinde açılırsa ardından gelen bütün düz anahtarlar
  (`tick_hz`, `room_count`, `max_snapshot_bytes`…) son girdiye düşerdi —
  F61'den önce **hepsi sessizce atılır, sunucu varsayılanlarla
  kalkardı**; şimdi başlatma bunlardan birini adlandırarak durur. Örneğe
  "girdileri düz anahtarlardan sonra, dosyanın sonuna yaz" kuralı
  eklendi (`example_config::the_commented_listener_example_is_valid`
  ikisini de kilitler).
- **Geriye uyumsuz, bilerek:** bugün girdide yutulan bir anahtar artık
  başlatmayı durdurur (bakımcı kararı (a)). Depodaki config'lerde böyle
  bir anahtar yoktu (örnek dosya, testlerin ve loadgen'in kurduğu
  girdiler yalnız dört anahtarı yazıyor).

**Başlatma hatası: tek mesaj, çıkış durumu 1 (BACKLOG F63 —
2026-09-28).** `gsb-server` başlatmayı durduran her hatayı — config
dosyası okunamadı ya da ayrışmadı, bir anahtarın değeri reddedildi
(`ServerError`), bir kapı kalkmadı — stderr'e tek mesaj olarak basar ve
1 ile çıkar:

```text
gsb-server: invalid `listen_backlog` 0: must be 1..=2147483647 (listen(2) takes a C int; …)
gsb-server: listener `127.0.0.1:7777` (tcp) did not start: Address already in use (os error 98)
gsb-server: listener `0.0.0.0:7443` (tls) did not start: cannot open `tls_cert` file `/etc/gsb/cert.pem`: No such file or directory (os error 2)
gsb-server: cannot read config file gsb.toml: Is a directory (os error 21)
```

- **Metin:** hatanın `Display`'i; `source()` zincirinde metnin zaten
  taşımadığı her neden ayrı bir `caused by: …` satırı
  (`gsb_server::error_chain` — kendi oyununu `start_game_server` ile
  barındıran bir ikili aynı biçimi kullanabilir). Bu depodaki hatalar
  nedenlerini kendi mesajlarına yazar, bugün ek satır çıkmaz; ek satır
  üçüncü taraf bir modülün kendi nedeni olan hatası içindir. Ayrıştırma
  hatası satırı ve işaretli alıntıyı taşır; dosyanın başka satırı
  basılmaz.
- **Önceden:** `main` hatayı döndürüyordu, standart kütüphane `Debug` ile
  basıyordu: `Error: Parse { path: …, source: Error { message: …,
  input: Some("<dosyanın tamamı>"), … span: Some(82..96) } }` — satır
  numarası yok, dosya dökülüyor; `Error: BadListenBacklog(0)`,
  `Error: Bind(Os { code: 98, kind: AddrInUse, … })` (hangi kapı
  olduğu yok). Çıkış durumu aynı: 1.
- **Kapı hatası kapıyı adlandırır:** `ServerError::ListenerBind { addr,
  transport, source }` — soket reddi (dolu port, soket kurucusunun
  reddettiği backlog) de TLS/QUIC dosyası yüklenemediği de. Adres
  config'te yazıldığı gibi (port 0 dahil). Kapıyı adlandırmayan eski
  `ServerError::Bind(io::Error)` kaldırıldı (hiçbir yol artık onu
  üretmiyordu).
- **`ConfigError`'ın `Debug`'u dosyasız:** elle yazıldı — ayrıştırma
  hatasında yol, mesaj ve bayt aralığı (`span`); `toml`'un hatasının
  sakladığı girdi kopyası basılmaz (`Config::from_file(..).unwrap()`
  paniği ya da `{:?}` log satırı dosyayı dökmez).
- **Yük üreteci de aynı:** `gsb-loadgen`'in süreç-içi sunucusu
  kalkmazsa `gsb-loadgen: the in-process server did not start: <hata>`
  ve çıkış durumu 1 (önceden `Debug`'lı panik, 101); `--serve` çocuğu
  zaten `Display` basıp 1 ile çıkıyordu, artık aynı zinciri basar.
  Komut satırı reddi değişmedi (çıkış durumu 2).

**Config tablolarının bilinmeyen anahtar denetimi (F61 taraması).**

| Tablo (struct) | Önce | Sonra |
|---|---|---|
| `[[listeners]]` girdisi (`ListenerEntry`) | bilinmeyen anahtar sessizce atılıyordu | **reddeder** (F61) |
| `[rooms.<id>]` (`RoomOverride`) | reddediyordu (B18) | değişmedi |
| `[rooms]` anahtarları (oda id'leri) | pozitif düz tam sayı olmayan id reddediliyordu | değişmedi |
| `[metrics]` (`MetricsConfig`), `[metrics.otlp]` (`OtlpSection`) | reddediyordu | değişmedi |
| `[arena]`, `[mmo]`, `[war]` (`games::settings::own_table`) | reddediyordu (GAME-MODULE §6 sapma 2) | değişmedi |
| Seçim ve taşıma enum'ları (`visibility`, `topology`, `communication`, `transport`, girdinin `transport`'u), `afk_action`, `max_detach_hold_secs` | bilinmeyen DEĞER reddediliyordu | değişmedi |
| Üst düzey (`Config`) | bilinmeyen anahtar yok sayılıyordu | **reddeder** (F62, başlatmada; aşağıda "Üst düzey") |

**Üst düzey: motorun ve oyunların anahtarları, gerisi reddedilir
(BACKLOG F62 — 2026-09-28, bakımcı kararı (b)).** TLS, ops HTTP
(`http_listen`), `listen_backlog` ve rUDP düğmeleri üst düzeyin düz
anahtarları; orası barındırılan oyunla PAYLAŞILAN ad alanı — oyun kendi
parçasını `Config::raw`'dan okur (`[<oyun>]` tablosu, demo'nun düz
anahtarları, üçüncü taraf modülün okuduğu anahtarlar) ve bir dosya
birkaç oyunun tablosunu taşıyabilir. Bu yüzden `Config` ayrıştırırken
bilinmeyen anahtarı reddetmez (`deny_unknown_fields` yok); sunucu her
başlatmanın **ilk** adımında, bir şey bağlanmadan, üst düzeyi denetler
(`Config::check_top_level_keys`). Bir üst düzey anahtar kabul edilir
ancak:

- **motorunsa** — `Config`'in alanlarından biri, dosyadaki yazımıyla
  (`max_detach_hold_secs`, `rooms`, `listeners`, `metrics`, `game` …).
  Liste struct'ın kendi türetilmiş `Deserialize`'ından okunur (serde'nin
  struct'a verdiği alan listesi): alan eklemek/adlandırmak listeyi
  kendiliğinden günceller, kayamaz. Demo'nun `Config`'te uyumluluk için
  duran düz anahtarları (`visibility`, `topology`, `communication`,
  `shard_count`, `aoi_cell_size`, `team_vision_radius`,
  `spawn_half_size`, `disconnect_grace_secs`) motorun DEĞİL, demo'nun;
  her birinin `Config` alanı olduğunu bir test kilitler;
- ya da **bir oyunun sahip olduğu anahtarsa** —
  `GameModule::owned_keys()` (sağlanan metot): barındırılan oyunun ya da
  bu ikiliye derlenmiş BAŞKA bir oyunun. Bir ad o adı taşıyan üst düzey
  anahtarın tamamına sahiptir: düz değer (`hiz = 3`) ya da tablo
  (`[ad]`, `[ad.alt]`, `[[ad]]`). Varsayılan oyunun adının tablosu
  (`[arena]`, `[mmo]`, `[war]`); demo düz anahtarlarını bildirir ve
  `[demo]` tablosuna sahip değildir.

Gerisi başlatmayı durdurur (`ServerError::UnknownKey`; ikili F63
biçiminde basar, çıkış durumu 1):

```text
gsb-server: unknown top-level config key `tik_hz` (did you mean `tick_hz`?): not a key of the server, and no game compiled into this build owns it (games' keys — demo: `visibility`, `topology`, …; arena: `arena`; mmo: `mmo`; war: `war`); a key nobody reads is refused, not ignored
```

- **Hata:** anahtarı dosyadaki yazımıyla adlandırır — düz değer
  `` `tik_hz` ``, tablo `` `[room.2]` `` / `` `[metric.otlp]` ``, tablo
  dizisi `` `[[listener]]` `` —, bir ya da iki harf uzaklıkta bilinen bir
  anahtar varsa onu önerir ve derlenmiş oyunların anahtarlarını sayar.
  **Satır numarası yok:** denetim başlatmada, ham tablo üstünde koşuyor
  (dosya metni orada yok; `Config`'e alan eklemek genel struct'ı
  kırardı); bir TOML belgesinde üst düzey anahtar tektir, adı yerini
  belirler. Bir seferde ilk bilinmeyen anahtar raporlanır (serde'nin
  `deny_unknown_fields`'ı gibi).
- **Kardeş oyun kuralı:** bir dosya bu ikiliye derlenmiş başka bir
  oyunun tablosunu taşıyabilir (demo'yu barındıran sunucu `[arena]`'yı
  kabul eder, demo ona bakmaz — GAME-MODULE G2 sapma 2). **Bu ikiliye
  derlenmemiş bir oyunun tablosu reddedilir:** yalnız `game-arena` ile
  derlenmiş bir ikili `[mmo]` taşıyan dosyayla kalkmaz; demo'suz bir
  ikili demo'nun düz anahtarlarını reddeder. Yazım hatası koruması,
  derlemeler arası paylaşılan dosyanın rahatlığından önce gelir; öyle
  bir dosya o derleme için tablo çıkarılarak yazılır.
- **Kodla kurulan config:** `raw` boş, denetim her zaman geçer (testler,
  yük üreteci; yük üretecinin `--mmo-crystallize`'ı `[mmo]` yazar, MMO
  derlenmişse kabul).
- **Kapsam:** yalnız üst düzey. Alt tablolar kendi katılığını taşır
  (yukarıdaki tablo); bir oyunun tablosunun İÇİNİ oyun denetler
  (`games::settings::own_table`).
- **Geriye uyumsuz, bilerek:** bugün yok sayılan bir üst düzey anahtar
  artık başlatmayı durdurur. Depodaki config'lerde böyle bir anahtar
  yoktu (`config.example.toml` ve bütün yorumlu bölümleri, testlerin
  dosyaları, yük üretecinin yazdıkları).

## 3. Tel/format detayları

- Metrik adlandırma: `gsb_registry_rooms`, `gsb_room_r1_steps_total`,
  `gsb_conn_frames_in_total` gibi `<alan>_<nesne>_<sayaç>_total`;
  histogramlar Prometheus summary/satır çiftiyle (p50/p99 hazır alanlardan)
- Oda başına (etiket `room="r<id>"`; sharded odada her shard kendi
  satırı, id `room << 16 | index`) küçük pakette (W2'de takım
  değişimi, B39'da yayın kareleri için) eklenen sayaç aileleri
  (hepsi kümülatif `counter`; gsb-metric satırında aynı adla, `_total`
  ve `gsb_room_` öneki olmadan):

  | Aile | Anlamı |
  |---|---|
  | `gsb_room_shipped_frames_total` | Bağlantılara gönderilen kareler (snapshot + özel; fan-out kopyaları) — `gsb_room_shipped_bytes_total`'ın kare sayısı (B39; DESIGN §12): datagram taşıması bayt kadar PAKET ile de sınırlıdır, `shipped_bytes/shipped_frames` = ortalama kare boyu |
  | `gsb_room_private_frames_total` | Bunların bağlantıya özel olanları (RPC cevapları, ack'ler, tek atımlık full'lar); `shipped_frames − private_frames` = fan-out'un yayın yarısı (B39) |

  **`shipped_*` anlamı DARALDI (B57, sayım turu 2):** `gsb_room_shipped_bytes_total`,
  `…_shipped_frames_total`, `…_private_frames_total` (ve net kapsamının
  `bytes_out_room`'u, bunların toplamı) önceden fan-out'un KURDUĞU
  batch'leri sayıyordu — dolu kanalda düşen (`dropped`) ya da kapalı
  kanalın reddettiği (`sends_closed`) batch de "gönderildi" görünürdü.
  Artık yalnız çıkış kanalının ALDIĞI batch sayılır (`room::Shipped`,
  başarılı `try_send`'den sonra; oda + shard). Seçenekler: (a) "kuruldu"
  anlamını koruyup belgelemek — adı yanlış bırakırdı ("shipped" gönderildi
  demek) ve düşen yükü iki kez gösterirdi (hem `dropped`'ta hem trafikte);
  (b) başarıda saymak — seçildi: ad anlamla aynı, kayıplar kendi
  sayaçlarında, maliyet yığında üç tamsayı. Düşme yokken değerler aynı.
  HELP'ler zaten "shipped" diyordu; altın metinler değişmedi.
  | `gsb_room_detach_forced_total` | `max_detach_hold` tavanının duran bir `may_release` vetosunu ezerek bitirdiği bekletmeler (`detach_expired_*`'ın alt kümesi; RECONNECT §17) |
  | `gsb_room_effects_applied_total` | Bu shard'ın oyununun otorite olarak uyguladığı uzak etkiler (CROSS-SHARD §4b) |
  | `gsb_room_effects_forwarded_total` | Göç etmiş hedefin yeni sahibine devredilen etkiler |
  | `gsb_room_effects_orphaned_total` | Hedefi artık olmayan etkiler |
  | `gsb_room_effects_dropped_total` | Yolda kaybolan etkiler: dolu yeniden deneme tamponu, kapalı link, hop sınırı, yaş sınırı |
  | `gsb_room_effects_refused_total` | Kaynakta reddedilen `emit`'ler (tick bütçesi bitti ya da hedef ödünç verilmiyor) |
  | `gsb_room_migrations_out_total` | Komşu shard'a devredilen entity'ler (kesinleşen gönderim) |
  | `gsb_room_migrations_in_total` | Komşudan gelip kurulan entity'ler |
  | `gsb_room_migrations_failed_total` | Dolu komşu gelen kutusunun reddettiği göç gönderimleri (sonraki tick yeniden denenir) |
  | `gsb_room_team_exports_total` | Registry'nin takım hub'ına kuyruklanan takım export'ları (CROSS-SHARD §8b; W2) |
  | `gsb_room_team_export_drops_full_total` | Registry çalışırken DOLU posta kutusunun reddettiği export'lar — gerçek kayıp (sonraki tick aynı kümeyi taşır) |
  | `gsb_room_team_export_drops_closed_total` | KAPALI posta kutusunun reddettiği export'lar: registry duruşta çıkmıştı, adımının ortasındaki shard bir kez daha export etti (F50) |
  | `gsb_room_team_export_records_total` | Kuyruklanan export'lardaki kayıtlar |
  | `gsb_room_team_over_cap_total` | Çekirdeğin mesaj başı tavanlarının (`TEAM_EXPORT_MAX_*`) kestiği kayıt/takım — çıkışta ve girişte |
  | `gsb_room_team_over_budget_total` | Oyunun takım başı export bütçesinin (kit: `with_team_budget`) kestiği kayıt — export çekirdeğe varmadan önce; oyunun politikası, yük altında beklenebilir (A29) |
  | `gsb_room_team_imports_total` | Uygulanan takım import'ları (hub'ın buraya ulaşan röleleri) |
  | `gsb_room_team_import_records_total` | Uygulanan import'lardaki kayıtlar |
  | `gsb_room_team_expired_total` | TTL'in düşürdüğü kaynak yuvaları (sessizleşmiş kaynak) |
  | `gsb_room_requests_refused_congested_total` | Tıkalı bağlantının (son batch'i düştü) yanıt borcu per-connection cap'e ulaşmışken **işlenmeden ve yanıtlanmadan** reddedilen RPC istekleri — F14'ün fırtına sınırı; F15'ten beri `…_rejected_conn_cap_total`'dan ayrı (orada yalnız yanıtlanan cap retleri). Satırda `req_refused=`; RPC-CONTROL-PLANE §3.1 |
  | `gsb_room_sends_closed_total` | Fan-out'un **zaten kapalı** bir bağlantıya denediği batch'ler (`try_send` → Closed): istemci soketini kapatmış (tipik: LEAVE sonucundan hemen sonra), oda ayrılışı/kopuşu henüz işlememiş — bağlantı sonu başına tek bir değil, (o bağlantıya tick başına denenen batch) × (soketin kapanışıyla ayrılışın/kopuşun işlenmesi arasındaki tick) — ölçülen spatial demo 1000 `--workers 1` 1,6–1,8, shard'lı MMO/savaş varsayılan worker'larla 0,9–1,9 (F59); istemcinin istediği bir kare kaybolmaz. B32'den beri `gsb_room_dropped_total`'dan ayrı: o artık yalnız DOLU kanalı (yavaş istemci — HELP'inin dediği) sayar. Oran göstergesi yok (bağlantı sonlarıyla orantılı; oranı ayrılış oranının bir katıdır). Satırda `sends_closed=` (`dropped_s=`'den sonra), loadgen telinde GSMI, `RESULT`'ta `sends_closed=` (`dropped=`'den sonra, her satırda); RPC-CONTROL-PLANE §8.2 |
  | `gsb_room_requests_dropped_unread_total` | Oturum bittiğinde (ayrılış, despawn eden kopuş, yeniden katılım, resume, girdi-boşta tavanının `leave_room` altında geride bıraktığı park) action kanalında **odanın henüz okumadığı** RPC istekleri — işlenmez, yanıtlanmaz (CONTROL READ'den önce koşar; ayrılıştan hemen önce gönderilenler). Oda defterini kapatan kova: gönderilen = yanıtlanan + retler + `req_refused` + bu. Satırda `req_unread=`; B36, RPC-CONTROL-PLANE §8.3 |
  | `gsb_room_requests_dropped_unbound_total` | READ'in bağlama çevirisinde düşen RPC istekleri: bağlama satırı olmayan (bayat — resume sonrası eski oturum) bağlantı adına çekilen istek; işlenmez, yanıtlanmaz. Yapısal olarak nadir (eski kanal yeniden bağlamada ölür). Defterin terimi. Satırda `req_unbound=` (`req_unread=`'den sonra), loadgen telinde GSML; B54 |
  | `gsb_room_actions_dropped_unread_total` | Oturum bittiğinde action kanalında okunmamış kalan DÜZ oyun girdileri (`req_unread`'in yerleri; istekler orada, bu ayrık) — işlenmez. Satırda `actions_unread=` (`team_expired=`'den sonra), loadgen telinde GSML, `RESULT`'ta `actions_unread=`; B54 |
  | `gsb_room_actions_dropped_unbound_total` | Bağlama çevirisinde düşen DÜZ oyun girdileri (bağlama satırı olmayan bağlantı; istekler `…_requests_dropped_unbound_total`'da). Satırda `actions_unbound=`; B54 |
  | `gsb_room_requests_undelivered_total` | Oda üretti ama oturum ÖNCE bittiği için hiçbir batch'in taşımadığı RPC **yanıtları** (ayrılış, kopuş/park, göç, yeniden katılım; oturumu aynı tick'te tablodan çıkan satır dahil) — atıldıkları yerde sayılır. İsteğin kendisi kendi kovasında zaten bir kez sayılı (`req_local`, bir ret, …); bu, yanıtına ne olduğunu sayar — defterin terimi DEĞİL. Düşen/kapalı batch'ten sonra kuyruğa geri konan yanıt (F14) yalnız sonunda atıldığında bir kez sayılır. Satırda `req_undelivered=` (`req_late=`'den sonra), loadgen telinde GSMK, `RESULT`'ta `req_undelivered=`; B53, RPC-CONTROL-PLANE §3.1 |
  | `gsb_room_requests_abandoned_total` | Oturumu bittiğinde hâlâ **uçuşta** (worker'da) olan dış RPC istekleri: o oturuma yanıt gitmeyecek. Worker'ın sonradan gelen raporu ayrıca `gsb_room_requests_late_total`'da (rapor sayacı, istek değil) sayılır; bu sayaç oturum sonundaki kaybı söyler. Satırda `req_abandoned=`, loadgen telinde GSMK, `RESULT`'ta `req_abandoned=`; B53 |

  Oda KAPANIRKEN (kapama/emeklilik/sunucu durması) kuyruktaki yanıtlar ve
  uçuştaki istekler aktörle birlikte gider ve bu iki aileye düşmez: oda
  dururken son örnek göndermez (RPC-CONTROL-PLANE §8.3, elenen 5 — geç
  örnek yok edilmiş odanın akümülatörünü diriltebilirdi), yani sayılsa da
  hiçbir rapora ulaşmazdı.

  Etki, göç ve takım aileleri yalnız shard satırlarında hareket eder
  (tek oda aktörü 0 yazar; takım ailesi yalnız `team_exchange`'i
  uygulayan mantıkta — `ShardedTeamRoom`). Takım sayaçlarının ~1 sn
  penceresi `team_exchange_summary` log satırında da; hub tarafının
  penceresi (`relays`, `relay_drops_full`, `relay_drops_closed`)
  `team_hub_summary` satırında, reddedilen röleleri B72'den beri registry
  kapsamında da (aşağıda). Crystallization olayları (kit) F9'dan beri
  aşağıdaki mantık sayaçlarıdır (`crystal_*`); wire başına ayrıntı
  `gsb_kit::crystal` debug satırında kaldı (CROSS-SHARD §4c madde 5).
- **Mantığın kendi sayaçları (F9).** Çekirdeğin bilmediği, oda
  mantığının (kit kompoziti ya da oyun) kendi adlandırdığı kümülatif
  sayaçlar; çekirdek yeni sayaç için değişmez. Oyun/kit sayacı bir
  `const` olarak bir kez bildirir —
  `LogicCounter::sum("war_kills", "…")` ya da `LogicCounter::max(..)`
  (yüksek-su işareti) — değerini kendi düz alanında tick içinde sayar ve
  `GameLogic::logic_counters` (kit: `Game::counters`) örnek başına bir
  kez hepsini (sıfırlar dahil) koyar. Ad kuralları: 1–32 bayt
  `[a-z0-9_]`, harfle başlar, `_total` ile bitmez (const değerlendirmede
  derleme hatası; `counters_dropped` ayrılmış). Oda başına en çok **16**
  ad; fazlası atılır, sayılır, aktör bir kez `warn` eder ve sayı
  aşağıdaki taşma anahtarında görünür (F17). Görünüm:

  | Yüzey | Biçim |
  |---|---|
  | `gsb-metric scope=room` satırı | çekirdek anahtarlarından SONRA, mantığın koyduğu sırayla `logic_<ad>=<değer>` |
  | Prometheus (SUM) | `gsb_room_logic_<ad>_total{room="r<id>"}`, `counter`, HELP = bildirimin help'i |
  | Prometheus (MAX) | `gsb_room_logic_<ad>{room="r<id>"}`, `gauge` (tepe bir oranın payı değildir) |
  | loadgen teli | `GSMC`: odanın kaydının sonunda sayı, taşma sayısı, sayaç başına ad + kural + değer (help taşınmaz) |
  | loadgen `RESULT` | `game=`'den hemen önce `logic_<ad>=<değer>` — koşunun son katlanmış raporundan, her sayaç kendi kuralıyla katlanmış |
  | Sınır taşması (F17) — yalnız sıfırdan büyükken | satırda ve `RESULT`'ta mantığın anahtarlarından sonra `logic_counters_dropped=<n>`; Prometheus'ta `gsb_room_logic_counters_dropped{room="r<id>"}`, `gauge` (odanın son örneğinde sığmayan ad sayısı); `counters_dropped` adı mantığa kapalı |

  Ad başına aile seçildi, `name` etiketli tek aile
  (`gsb_room_logic_total{name="kills"}`) elendi: tek aile hem `counter`
  hem `gauge` olamaz ve sayaç başına HELP kaybolurdu; kardinalite iki
  yolda da aynı (adlar oyun başına statik, oda etiketi zaten her oda
  ailesinde). Sayaç bildirmeyen bir mantığın `gsb-metric` ve Prometheus
  metni **bayt bayt aynıdır** (çekirdek testi
  `metrics::tests::golden` önceki kodun ürettiği metne sabit). Bugün
  bildirenler: sharded kit odaları crystallization açıksa altı
  `crystal_*` (`moves`, `release_quiet/band/partner`, `untracked`,
  `fights_peak` — MAX), savaş demosu `war_kills`; savaş ve MMO demoları,
  vuruş beslemesi (`set_combat_feed`) takılıysa, beslemenin alamadığı
  vuruşları `combat_hits_dropped_full` (besleme dolu) ve
  `combat_hits_dropped_closed` (okuyucu gitmiş) olarak (B81) — bu ikisi
  F17'nin kuralıyla yalnız sıfırdan büyükken konur (düşürmeyen bir
  beslemenin satırı öncekiyle aynı). `/rooms` sayaç listelemez (yalnız
  oda id'leri), değişmedi.
- **Sunucu kapanışları: `idle_input` (E6).** `server_closes` ailesine
  (`gsb_net_server_closes_total{reason}`, OTLP'de `gsb_net_server_closes`)
  SONA eklenen etiket: odanın girdi-boşta tavanı `afk_action = disconnect`
  altında kapattığı oturumlar. Log satırında `server_close_idle_input=`,
  loadgen metrik telinde `GSMF` (`server_closes` dizisinin son slotu;
  dizi `ServerClose::COUNT` uzunlukta olduğundan yeni sebep = yeni
  düzen). Varsayılanda hep 0. `idle_timeout`'tan ayrı: o taşıma-boşta
  (hiç bayt yok), bu girdi-boşta (heartbeat var, oyun girdisi yok).
- **Sunucu kapanışları: `kicked` (E8).** `server_closes` ailesine
  `idle_input`'tan sonra SONA eklenen etiket: oyun mantığının attığı
  oturumlar (`TickCtx::kick` / `gsb_kit::game::kick`; RECONNECT §16.3).
  Log satırında `server_close_kicked=`, Prometheus'ta
  `gsb_net_server_closes_total{reason="kicked"}`, OTLP'de
  `gsb_net_server_closes` serisinde `reason=kicked` noktası (seri sayısı
  13), loadgen metrik telinde `GSMG`, RESULT'ta `server_close_kicked=`
  (her sebep için bir anahtar kuralı). Hiç atmayan oyunda hep 0. İstemci
  aynı kapanışı `ERROR 9` + `kicked: <gerekçe>` olarak okur; gerekçe
  oyunundur (≤ 256 bayt).
- **Net kapsamı: girdi hız sınırı (E1).** Odanın hız sınırını aşıp
  bağlantı aktöründe düşürülen geçerli oyun girdisi:
  `gsb-metric scope=net` satırında `violations=`'dan hemen sonra
  `input_rate_limited=<n>`; aile tablosunda
  (`metrics/export/families/server.rs`, `NET`) `gsb_net_violations_total`'dan
  hemen sonra tek bir girdi, yani Prometheus'ta
  `gsb_net_input_rate_limited_total` (`counter`, kümülatif, bütün
  bağlantılar) ve OTLP'de `gsb_net_input_rate_limited` (monotonic `Sum`)
  birlikte (iki altın dosya da sabitliyor, `otlp::cross` uyumu kilitliyor);
  loadgen metrik telinde `GSME` (net kapsamında
  `violations`'dan sonra bir `u64`). Kaynağı `ConnSample::input_rate_limited`
  (bağlantı başına delta; toplayıcı toplar). Sınır kapalıyken (varsayılan)
  hep 0. Artış sınırın ÇALIŞTIĞINI söyler — ihlal değildir
  (`violations` ayrı), girdi kanala hiç girmediği için `actions_dropped`
  da artmaz. Bağlantıya atıf yok (`actions_dropped_top` gibi bir ilk-beş
  listesi eklenmedi): her bağlantı ilk düşürmede bir kez `warn` eder
  (bağlantı id'si + eş adresi); ayrı bir bağlantı başı tablo toplayıcıda
  bellek ve budama işi olurdu, ihtiyaç görülünce eklenir.
- **Net kapsamı: üyelik bittikten sonraki iletim (B51).** Oda üyeliği
  KENDİSİ bitirdiğinde (oyunun atması, girdi-boşta tavanı, oda
  kapanışı/emekliliği) oturumun action kanalı önce kapanır; bağlantı
  aktörü bunu registry'nin bildirimiyle (`LeftRoom`, `ServerClosed`,
  `RoomGone`) öğrenir. Arada ilettiği kare kapalı kanala çarpar ve
  odaya hiç ulaşmaz — sayıldığı yer bağlantı aktörü, iki ayrık sayaç:
  RPC istekleri (`RPC_REQ`) `requests_dropped_closed`, oyun-bandı
  girdileri `actions_dropped_closed` (ayrık; dolu kanalda da B55'ten beri
  aynı ayrım: `actions_dropped` / `requests_dropped_full`). `gsb-metric scope=net` satırında
  `input_rate_limited=`'den hemen sonra `actions_dropped_closed=<n>
  requests_dropped_closed=<n>`; aile tablosunda (`NET`)
  `gsb_net_input_rate_limited_total`'dan hemen sonra
  `gsb_net_actions_dropped_closed_total` ve
  `gsb_net_requests_dropped_closed_total` (`counter`, kümülatif, bütün
  bağlantılar; OTLP'de `_total`'sız monotonic `Sum`); loadgen metrik
  telinde `GSMJ` (net kapsamında `input_rate_limited`'den sonra iki
  `u64`); `RESULT`'ta `actions_dropped_top=`'tan sonra iki anahtar, her
  satırda. Kapalı iletim bağlantıyı odadan ayırır (sonraki kare `ERROR
  6` alır ve yarış sınıfı ihlal olarak `violations`'ta sayılır), yani
  biten üyelik başına en çok bir kare buraya düşer. RPC defterinin bağlantı tarafı terimi (RPC-CONTROL-PLANE
  §8.3); loadgen'de hep 0 (üyeliği hep istemci bitirir).
- **Net kapsamı: RPC defterinin bağlantı tarafındaki iki kenarı (B55) ve
  `actions_dropped`'ın DARALAN anlamı.** Bağlantının `try_send`'i DOLU
  action kanalına çarptığında RPC isteğini (`RPC_REQ`) artık
  `requests_dropped_full`'da sayar; **`actions_dropped`
  (`gsb_net_actions_dropped_total`, `actions_dropped_top`) yalnız
  oyun-bandı girdisini sayar** — sayım turu 2'ye dek istekleri de
  sayıyordu. Adı ve HELP'i ("Game-band input actions …, RPC requests are
  counted apart") artık aynı şeyi söylüyor; bu, ailenin tek bilinçli HELP
  değişikliği (altın metinler). Odası olmayan bağlantıya gelen istek
  (hiç katılmadı, ayrıldı ya da üyeliği bitti; kimlik doğrulamadan önce
  dahil) `ERROR 6` ile yanıtlanır (ihlal yanıt sınırı içinde; sonra
  sessiz) ve yarış sınıfı ihlal olarak `violations`'ta sayılmaya DEVAM
  eder; ayrıca `requests_no_room`'da bir kez sayılır (ihlal sayacı
  değişmedi, defter kendi terimini aldı). `gsb-metric scope=net`
  satırında `requests_dropped_closed=`'dan sonra `requests_dropped_full=
  requests_no_room=`; aile tablosunda (`NET`)
  `gsb_net_requests_dropped_full_total`, `gsb_net_requests_no_room_total`
  (OTLP'de `_total`'sız); loadgen telinde `GSMM`; `RESULT`'ta
  `requests_dropped_closed=`'dan sonra, her satırda. Defter
  (RPC-CONTROL-PLANE §8.3): `rpc_sent = req_local + req_ext + Σ req_rej_*
  + req_refused + req_unread + req_unbound + requests_dropped_closed +
  requests_dropped_full + requests_no_room` — her istek tam olarak bir
  terimde (sayım turu 3'ten beri `+ requests_unprocessed`, B60, ve rUDP'de
  taşımanın terimi, B58 — §8.3).
- **Net kapsamı: heartbeat kısmasının fazlası (B56).** Saniyede birden
  fazla gelen heartbeat'in cevaplanmayanları (SECURITY §3.2) faza göre:
  kimlik doğrulamadan önce `heartbeats_throttled_preauth` (güvenlik
  sinyali), sonra `heartbeats_throttled_authed` (istemcinin heartbeat
  zamanlayıcısı hızlı). İhlal değil, oturumu canlı tutar. Satırda
  `requests_no_room=`'dan sonra `hb_throttled_preauth= hb_throttled_authed=`;
  aile tablosunda `gsb_net_heartbeats_throttled_preauth_total`,
  `gsb_net_heartbeats_throttled_authed_total`; loadgen telinde `GSMN`;
  `RESULT`'ta aynı anahtarlar. Önceden yalnız debug satırındaydı.
- **Net kapsamı: bağlantı aktörünün kendi çıkış kayıpları (B57) ve
  `frames_out`/`bytes_out_control`'ün DARALAN anlamı.** Aktörün kontrol
  kareleri (AUTH/JOIN sonuçları, ACK'ler, `ERROR`'lar) odanın fan-out
  ettiği aynı sınırlı çıkış kanalından gider. `send_frame` kareyi
  göndermeden ÖNCE sayıyordu: kapalı kanalın (yazıcı pompası gitmiş)
  reddettiği kare de "gönderildi" görünürdü. Artık kanal kareyi aldıktan
  SONRA sayılır; `gsb_net_frames_out_total` / `gsb_net_bytes_out_control_total`
  (ve `bytes_out_total`) yalnız kuyruğa girenleri söyler — adlarının
  dediği ("sent"). Reddedilen kare `frames_out_closed`'da
  (`gsb_net_frames_out_closed_total`): bekleyen gönderimde oturum ilk
  reddte biter (`outbound_dead` ya da benimsenen karar), en iyi çaba
  kapanış bildirimi (`try_notice`: sunucu durması `ERROR 14`, akış reddi,
  odanın atma/boşta kapanışı `ERROR 9`) de kapalı kanalda buraya düşer.
  Aynı bildirim DOLU kanalda (istemci okumuyor) düşerse
  `close_notices_dropped`'da (`gsb_net_close_notices_dropped_total`) —
  istemci kapanışı gerekçesiz alır. Satırda `hb_throttled_authed=`'dan
  sonra `frames_out_closed= close_notices_dropped=`; loadgen telinde
  `GSMO`; `RESULT`'ta aynı anahtarlar. Bayt tanımı: kare GÖVDESİ
  (2 baytlık op + payload; taşımanın çerçevesi — uzunluk öneki, WS/TLS
  başlığı — hariç). `bytes_out_control` kanalın aldığı karelerin bu
  gövdelerinin TAM toplamı; reddedilen kare ve düşen bildirim içinde
  değil (F2, kilit `conn_counts::bytes`: kanalın teslim ettiği karelere
  karşı bayt bayt).
- **Net kapsamı: sunucu kararlı sonun işlenmeden bıraktığı kareler
  (B60).** Sunucu oturumu kendisi bitirdiğinde (pompanın ya da
  registry'nin hükmü, odanın atması/boşta kapanışı, yok edilen oda,
  sunucu durması, ihlal bütçesi, ölü çıkış yolu) aktör gelen kutusunu
  okumayı bırakır; okuyucunun hükmün ARKASINA kuyruğa koyduğu kareler
  hiç işlenmez. Aktör çıkarken kutuyu kapatır (`close()` — yeni gönderim
  olmaz, boşaltma kapasiteyle sınırlı) ve kalanları türüne göre sayar;
  ölü çıkış yolunun `adopt_pending_close` taramasında gördüğü kareler de
  (önceden sessizce atılıyordu) ve pre-auth bütçesini AŞAN kare (sayılır
  ama işlenmez) aynı sayaçlarda: `requests_unprocessed`
  (`gsb_net_requests_unprocessed_total` — RPC isteği, hiç yanıtlanmaz;
  RPC defterinin terimi), `actions_unprocessed`
  (`gsb_net_actions_unprocessed_total` — oyun bandı, kayıtlı olsun
  olmasın), `control_frames_unprocessed`
  (`gsb_net_control_frames_unprocessed_total` — istek dışındaki temel
  bant: AUTH, JOIN, LEAVE, HEARTBEAT, tanımsız temel opcode). Tür ayrımı
  tek yerde: `gsb_core::conn::FrameKind`. **Anlam notu:** `frames_in`
  aktörün kutudan ALDIĞI kareyi sayar; kutuda kalanlar `frames_in`'de
  YOK, bütçeyi aşan kare ise var (alındı, işlenmedi). İstemci-tarafı son
  (okuyucunun `Closed`'u son mesajıdır) arkasında kare bırakmaz; boşaltma
  yine koşar (boşken tek `try_recv`). Kutu kapandıktan sonra okuyucu
  pompasının göndermeye çalıştığı kare taşıma tarafındadır (B66'dan beri
  `transport_stream_*_dropped_closed`, aşağıda). Satırda `close_notices_dropped=`'dan sonra
  `requests_unprocessed= actions_unprocessed= control_frames_unprocessed=`;
  loadgen telinde `GSMQ`; `RESULT`'ta aynı anahtarlar, her satırda.
- **Taşıma kapsamı: ağ katmanının kendi kayıpları (B58).** Yeni bir
  kapsam: `gsb-metric scope=transport` satırı (net satırlarından sonra,
  her anahtar her zaman, sıfırlar dahil) ve aile tablosunda
  (`metrics/export/families/transport.rs`, `TRANSPORT`, server-closes
  ailesinden sonra) 18 `counter`; OTLP'de `_total`'sız, loadgen telinde
  `GSMR` (ilişkilendirme listesinden sonra, alan sırasıyla), `RESULT`'ta
  `transport_<ad>=` (her satırda). Bütün kapılar birlikte (kapı başı
  ayrım görev sonu log'larında). Her ad saydığını söyler:
  `udp_requests_dropped_full` / `udp_actions_dropped_full` /
  `udp_control_frames_dropped_full` — rUDP demux'ının dolu oturum
  kutusunda düşürdüğü kareler, türüne göre (`conn::FrameKind`; önceden
  oturum başı tek sayı, yalnız uyarı ve log). Güvenilir bantta gelen
  kare zaten ACK'lenmiştir, istemci yeniden göndermez: istek terimi RPC
  defterinin TAŞIMA terimidir (RPC-CONTROL-PLANE §8.3);
  `udp_acks_not_forwarded` — yazıcıya verilemeyen gelen ACK (çıkış
  kanalı dolu; eş yeniden sorar; eski adı `ack_piggyback_failed`);
  `udp_datagrams_oversized` (`oversized_in`), `udp_datagrams_malformed`
  (`bad_datagrams`), `udp_bad_cookies` (`bad_cookie`),
  `udp_frags_refused` (`frag_refused`),
  `udp_sessions_dropped_accept_full` (`endpoints_dropped`: accept
  kuyruğu dolu, oturum düştü; istemcinin proof'u yeniden dener);
  yazıcıdan `udp_frames_dropped_oversized` (`dropped_oversized`: parça
  tavanını aşan oyun karesi), `udp_control_frames_abandoned`
  (`abandoned`: bant ölünce ACK'siz kalan kontrol kareleri),
  `udp_frames_drained` (`drained`: oturum bittikten sonra kanaldan alınıp
  hiç gönderilmeyen OTURUM kareleri — oyun ve kontrol; **anlam B73'te
  daraldı:** önceden `batch.len()` sayılıyordu, aynı kanaldan yazıcıya
  giden demux'ın ACK taşıması (`UDP_ACK`) da giriyordu; o oturumun karesi
  değil, biten bandın taşıma mesajıdır — `udp_frames_unsent` gibi artık
  sayılmaz, HELP bunu söyler; ad korundu, çünkü ad "kare" diyor ve
  sayaç artık yalnız onu sayıyor); WS okuyucusundan
  `ws_close_frames_dropped` (kapanış yankısı ya da protokol hatası
  kapanışı) ve `ws_pongs_dropped` — dolu kontrol kuyruğunda (önceden
  `let _ =`; kapalı kuyruğunkiler ayrı: B83, aşağıda); el sıkışan kapılardan (WS/TLS/QUIC)
  `handshakes_refused`, `handshakes_timed_out`, `handshakes_failed`
  (`Listener::handshake_stats`'ın sayaçları); ve `metrics_dropped` —
  taşıma görevlerinin dolu metrik kanalında düşen kendi örnekleri (üst
  düzey `gsb_metrics_dropped_total`'a da katılır; deltaları sonraki
  örnekte). **Tazelik:** görev uyandığında (datagram, kare, accept) en
  çok 500 ms'de bir ve biterken gönderir; sessiz bir demux son sayılarını
  bir sonraki datagramına ya da bitişine dek taşır; intake'in zaman
  aşımı/başarısızlığı bir sonraki accept'te ya da kapı kapanınca gelir.
  **Sayılmayan:** istemci tarafı parça sayaçları (`frag_rejected`,
  `frag_dropped_incomplete`) sunucunun değil istemcinin kaybıdır
  (`UdpClientStats`, loadgen `frag_dropped=`); rUDP'nin OOB penceresinde
  düşen gelen REL karesi (`oob_dropped`) istemcinin yeniden gönderimiyle
  geri gelir (kayıp değil, gecikme); kalanlar BACKLOG'da.
- **Taşıma kapsamı: akış pompalarının kayıpları (B66, sayım turu 4).**
  Satırın ve tablonun SONUNA (`metrics_dropped`'tan sonra; mevcut
  değerler kaymasın diye) yedi `counter`; loadgen telinde `GSMS`,
  `RESULT`'ta `transport_<ad>=`. Her akış kapısı (TCP/TLS/QUIC/WS; düz
  TCP de: `TcpTransport::metrics`) sayar:
  `stream_frames_unwritten` (`gsb_transport_stream_frames_unwritten_total`)
  — kapının ALIP hiç yazmadığı çıkış kareleri, yazıcısı önce durduğu
  için: yazıcı pompası başarısız yazma ya da yazma tıkanmasıyla bitince
  yazdığı batch'in kalanı (düşen/tıkanan kare dahil) ve çıkış kanalında
  hâlâ duran her batch'in kareleri; WS'nin soket yazıcısı başarısız soket
  yazması ya da eşin kapanış el sıkışmasıyla durunca kuyruğundaki oyun
  kareleri. **Anlam notu:** oda (`shipped_*`) ve bağlantı aktörü
  (`frames_out`) bunları kanal aldığı için saymıştı — sayaç "kanal aldı,
  soket hiç görmedi" farkıdır; olağan son (bütün göndericiler gitti)
  hiçbir şey bırakmaz. `stream_batches_unwritten` — yazıcı pompası öyle
  bittiğinde kanalda kalan batch sayısı (kareleri yukarıdakinde).
  `stream_requests_dropped_closed` / `stream_actions_dropped_closed` /
  `stream_control_frames_dropped_closed` — okuyucu pompasının elindeki
  kare, sunucu kararlı son aktörün kutusunu kapattığında (B60) reddedildi;
  bağlantı başına en çok bir kare, türüne göre (`conn::FrameKind`); istek
  terimi RPC defterinin terimidir (RPC-CONTROL-PLANE §8.3), aktör onu hiç
  almadığından `requests_unprocessed` ile çakışmaz.
  `ws_control_frames_unwritten` — WS soket yazıcısının aynı durmada
  kuyrukta bıraktığı kontrol kareleri (pong, kapanış; sunucunun kapanışı
  henüz gitmemişken). `ws_frames_dropped_after_close` — gönderilmiş bir
  kapanış çerçevesinin ardındaki oyun kareleri (RFC 6455 §5.5.1: kapanıştan
  sonra veri yok; reddedilen akışta bağlantının bildirimi ve uçuştaki
  fan-out); oradaki kontrol karesi kuraldır, sayılmaz. Pompalar sayıyı
  sonlarında bir kez gönderir (kayıp varsa; son örnek kuralı: dolu kanalda
  doğurulan göndericiyle).
- **Taşıma kapsamı: rUDP'nin kalan kayıpları ve ertelenen hükümler (B66,
  sayım turu 4).** Akış sayaçlarının ardına on `counter` daha; loadgen
  telinde `GSMT`. Yazıcıdan: `udp_game_datagrams_send_failed` — soketin
  reddettiği oyun bandı datagramı (RAW ya da bir FRAG parçası; kayıp,
  yeniden gönderilmez); `udp_control_datagrams_send_failed` — soketin
  reddettiği güvenilir kontrol datagramı, ilk gönderim ya da yeniden
  gönderim (bant canlılık sınırına dek yeniden dener; kayıp değil,
  reddedilen deneme — önceden yalnız debug satırı);
  `udp_frames_unsent` — bant ölünce (`die`) hiç gönderilmeyen kareler:
  gönderilen batch'in kalanı (taşınamayan kontrol karesi dahil) ve çıkış
  kanalında duran her kare (demux'ın ACK taşıması sayılmaz; oda/bağlantı
  bunları "gönderildi" saymıştı). Demux'tan: `udp_acks_send_failed` —
  soketin reddettiği birikimli ACK (el sıkışmanın kabulü dahil; istemci
  yeniden gönderir), `udp_challenges_send_failed` — reddedilen el sıkışma
  sorusu; `udp_requests_dropped_closed` / `udp_actions_dropped_closed` /
  `udp_control_frames_dropped_closed` — bağlantı aktörü kutusunu kapatmış
  bir oturuma çözülen kare (`Closed` kolu; oturum hemen silinir), sıradaki
  güvenilir kareler dahil, türüne göre; istek terimi RPC defterinin
  terimidir (henüz ACK'lenmemişti ama yeniden göndereceği oturum artık
  yok); `udp_datagrams_no_session` — oturumu olmayan adresten gelen REL,
  RAW ve ACK datagramları (oturumu bitmiş ya da hiç kurulmamış; çözülmeden
  atılır). Ve `writer_verdicts_deferred` — bir yazıcının hükmü (akış
  pompasının yazma tıkanması, rUDP bandının ölümü) doğumda ayrılmış slot
  olmadan dolu posta kutusuna denk geldi: çıkış kanalı kapandıktan SONRA
  teslim edilir, kapanış `outbound_dead` diye kaydedilebilir. **B66'nın
  düzeltmesi:** rUDP'nin `die`'ı artık akış pompasının ayrılmış slotunu
  (`pump::verdict`) kullanır — yazıcı doğarken posta kutusundan bir slot
  ayırır; `RelDead` bildirimi dolu kutuda da oradadır (önceden `try_send`
  düşer, kapanış `outbound_dead` sayılırdı). Bu sayaç yalnız doğumda slot
  ayrılamayan (kutu zaten dolu) nadir yolu sayar. Bedel: rUDP oturumu
  başına bir posta kutusu slotu (akış kapılarındaki gibi).
- **Taşıma kapsamı: kapanan kapının ve rUDP accept tarafının kayıpları
  (B74, sayım turu 5).** Satırın ve tablonun sonuna dört `counter`;
  loadgen telinde `GSMX`, `RESULT`'ta `transport_<ad>=`. El sıkışan
  kapılardan (WS/TLS/QUIC): `handshakes_cut_closed` — kapı kapanırken
  uçuşta olan, kesilen el sıkışmalar (bağlantı el sıkışmadan kapanır;
  önceden yalnız debug satırı); `handshakes_unaccepted_closed` — BİTMİŞ
  ama uç noktası accept döngüsü için kuyrukta beklerken kapı kapanan el
  sıkışmalar: atıldı, hiç oturum olmadı (kapının `completed`'ında da
  sayılı; önceden sessizce boşaltılıyordu). `Listener::handshake_stats`
  ikisini `cut_closed` / `unaccepted_closed` olarak da verir; intake'in
  son örneği kapının oturmasını bekler (en çok 1 sn; DESIGN §6). rUDP:
  `udp_sessions_dropped_accept_gone` — kanıt doğrulandı ama accept tarafı
  gitmiş (dinleyici kapatılmadan düşürülmüş): oturum sökülür, kabul
  gitmez (`…_accept_full`'un eşi; önceden sayılmıyordu);
  `udp_sessions_unaccepted_closed` — kabulü istemciye GİTMİŞ ama uç
  noktası accept kuyruğundayken dinleyici kapanan ya da giden oturumlar
  (istemci bağlı sanar, sessizlikle öğrenir). **Kalan uç:** toplayıcı
  gitmişse (süreç inerken) sayı hiçbir yere varmaz — her son örneğin
  sınırı.
- **Taşıma kapsamı: WS'nin teslim edilemeyen kapanış çerçevesi (B80,
  sayım turu 6).** Satırın ve tablonun sonuna iki `counter`; loadgen
  telinde `GSMZ`, `RESULT`'ta `transport_<ad>=`. Sunucu oturumu
  bitirince WS kapısı 1001 "Going Away" kapanış çerçevesini soket
  yazıcısının kuyruğuna koyar. Önceden bu bir `try_send`'di: kuyruk
  DOLUYSA (aktörün son batch'i yavaş okuyan istemciye hâlâ gidiyor)
  çerçeve sayılmadan düşüyor, istemci hiç kapanış görmüyor, bağlantı o
  kapatana dek asılı kalıyordu. **Karar: teslim.** Kapanış artık oyun
  karesi gibi slot BEKLER (`poll_close` bekler; yazıcı pompası kapanışı
  yazma-tıkanma penceresi altında bekler — öteki kapıların soketi
  boşaltan kapanışıyla aynı sınır): soket boşaldıkça 1001 önündeki
  karelerin ARKASINDAN gider — her zaman amaçlanan aynı bayt (`88 02 03
  E9`), yalnız artık kaybolmuyor. (B30'dan beri hükümle biten oturumda
  aynı kapanış 1008 ya da 1013 taşır — DESIGN §5.6 "WS kapanış kodu";
  iki sayaç kodu ne olursa olsun sunucunun bu kapanışını sayar, adlarındaki
  "going_away" B24'ten kalma.) Teslim edilemeyen sayılır:
  `ws_going_away_unsent_closed` — kuyruk kapalı, soket yazıcısı başarısız
  bir soket yazmasıyla zaten durmuş; `ws_going_away_unsent_stalled` —
  kapanış slot beklerken bırakıldı, pompanın tıkanma penceresi bayt
  yazılmadan doldu (pencere kapalıysa kapanış slotu sonuna dek bekler, bu
  sayaç artmaz). Okuyucunun kendi kapanışı (istemcinin kapanışına yankı,
  protokol hatası kapanışı) önce ya da bu arada kuyruklandıysa bağlantının
  TEK kapanışı odur: 1001 gönderilmez, sayılmaz (kaybı okuyucunun
  `ws_close_frames_dropped`'ı). Yan düzeltme: soket yazıcısının erken
  çıkıştaki boşaltması `recv` ile bekler — kapanıştan önce slot ayırmış
  bir göndericinin (uçuştaki kare, bekleyen kapanış) kapalı kuyruğa
  koyduğu kare de sayılır (`try_recv` boş kuyrukta durup onu kaçırırdı).
- **Taşıma kapsamı: WS okuyucusunun kapalı kuyruğa veremediği kontrol
  cevapları (B83, sayım turu 7).** Satırın ve tablonun sonuna iki
  `counter`; loadgen telinde `GSNA` (M dizisi GSMZ'de bitti, üçüncü harf
  ilerledi), `RESULT`'ta `transport_<ad>=`. Okuyucunun `queue_control`'ü
  (pong, kapanış yankısı, protokol hatası kapanışı) önceden yalnız DOLU
  kontrol kuyruğunu sayıyordu (`ws_close_frames_dropped`,
  `ws_pongs_dropped`, B58); soket yazıcısı başarısız bir soket yazmasıyla
  durup kuyruğunu KAPATTIKTAN sonra okuyucu hâlâ okurken gelen ping'in
  pong'u ya da kapanışın yankısı sessizce düşüyordu. Artık ayrı sayılır:
  `ws_close_frames_dropped_closed`
  (`gsb_transport_ws_close_frames_dropped_closed_total`) ve
  `ws_pongs_dropped_closed` (`gsb_transport_ws_pongs_dropped_closed_total`).
  Değeri düşük: soket zaten ölü, hiçbir cevap bir tele varamazdı; sayaç
  "bu oldu" der, istemcinin bir şey kaçırdığını değil. Dolu kuyruğun
  sayaçları anlam değiştirmedi (yalnız dolu). Okuyucunun `Shutdown`
  isteğinin reddi kare değildir, sayılmaz.
- **Taşıma kapsamı: rUDP kontrol bandının yeniden gönderimleri, sebebe
  göre (B2).** Satırın ve tablonun sonuna bir `counter`:
  `udp_control_retransmits_timeout`
  (`gsb_transport_udp_control_retransmits_timeout_total`); loadgen
  telinde `GSNF`, `RESULT`'ta `transport_udp_control_retransmits_timeout=`
  (her satırda). rUDP yazıcılarının, zamanlayıcısı ACK gelmeden dolduğu
  için yeniden gönderdiği güvenilir bant kareleri — önceden yalnız yazıcının
  oturum sonu log'unda (`retransmits`). Kendi başına kayıp DEĞİL (kare
  yine teslim edilir); ya bir kaybın (kare ya da ACK) ya da yolun
  turundan kısa bir zamanlayıcının işareti. B2'den beri zamanlayıcı
  uyarlanıyor (RTT tahmini, geri çekilme — DESIGN §6 "Yeniden gönderim
  zamanlayıcısı"), dolayısıyla sağlıklı bir yolda sayaç kayıpla orantılı
  kalmalı: kayıpsız bir yolda sürekli artıyorsa zamanlayıcı yolun turunu
  izleyemiyor demektir. Sebep adda: bugün her yeniden gönderim bir
  zamanlayıcı dolması (bantta hızlı yeniden gönderim yok); başka bir
  sebep kendi sayacını alır (B1'in tıkanıklık denetimi bu sayacı kayıp
  sinyali olarak okuyacak). İstemci tarafı karşılığı loadgen
  `retrans_out` (değişmedi).
- **Taşıma kapsamı: ops HTTP yüzeyinin iki sınırı (B49).** Satırın ve
  tablonun sonuna (D11'in ikisinden sonra) iki `counter`; `RESULT`'ta
  `transport_<ad>=` (her satırda; loadgen sunucusunun ops yüzeyi yoksa
  0). Loadgen teli: D11'in ikisiyle birlikte **GSNG**. `ops_http_conns_refused`
  (`gsb_transport_ops_http_conns_refused_total`) — `http_max_connections`
  canlı görev varken hemen, yanıtsız kapatılan bağlantılar;
  `ops_http_writes_timed_out` (`gsb_transport_ops_http_writes_timed_out_total`)
  — yazması `http_write_timeout_secs`'i aşan yanıtlar (bağlantı kapandı).
  Ops yüzeyi bir taşıma kapısı değil ama aynı `Door`'dan geçen ve aynı
  taşıma kanalına (`gsb_net::Flusher`) raporlayan bir kapı: accept
  döngüsü en çok 500 ms'de bir (bir accept'ten sonra) ve kapanırken
  gönderir; son accept'ten sonraki zaman aşımı sonraki accept'te ya da
  kapanışta gelir (intake'in kuralı).
- **Taşıma kapsamı: el sıkışan kapıların kaynak başına sınırı (D11).**
  Satırın ve tablonun sonuna iki `counter`; `RESULT`'ta
  `transport_<ad>=` (her satırda). Loadgen teli: taşıma bölümü
  `TRANSPORT_COUNT` uzunluğunda — yeni düzen, **GSNG** (B49'un ikisiyle
  birlikte). `handshakes_refused_per_source`
  (`gsb_transport_handshakes_refused_per_source_total`) — kaynağı (IPv4
  adresi, IPv6 /64) `max_handshakes_per_source`'u tutan bağlantılar, el
  sıkışmasız kapatıldı (QUIC: `refuse`); kapının kendi sınırının reddi
  `handshakes_refused`'ta kalır, ikisi karışmaz.
  `handshakes_retried_per_source`
  (`gsb_transport_handshakes_retried_per_source_total`) — yalnız QUIC:
  adresi henüz kanıtlanmamış kaynak sınırdayken reddedilmez, durumsuz
  bir Retry alır (yuva tutmaz); ret değil, ayrı sayaç (SECURITY §4.3.1
  #5). Sınır yazılmamışsa ikisi de hep 0. Kapı başına dökümü
  `Listener::handshake_stats()` (`refused_per_source`,
  `retried_per_source`) ve kabul görevinin kapanış özeti verir.
- **Oda kapsamı: takım export'unun reddi sebebe göre (F50).** Tek sayaç
  `team_export_drops=` / `gsb_room_team_export_drops_total` dolu ve
  kapalı registry posta kutusunu karıştırıyordu; iki ayrı ada bölündü,
  eskisinin yerinde: `team_export_drops_full=` /
  `gsb_room_team_export_drops_full_total` — registry ÇALIŞIRKEN dolu kutu,
  gerçek kayıp (A13/A25 tetiği bu); `team_export_drops_closed=` /
  `gsb_room_team_export_drops_closed_total` — kutu kapalı: registry
  duruşta odaları beklemeden çıkar (DESIGN §9), adımının ortasındaki
  shard bir kez daha export eder; bir tele varamazdı. **Yeniden
  adlandırma, daraltma değil:** eski ad kaldırıldı — aynı adla anlamı
  sessizce değişen bir sayaç, onu okuyan panoyu ve tetiği yanıltırdı;
  eski adı okuyan sorgu artık boş döner. Log satırı
  (`team_exchange_summary`) `export_drops_full` / `export_drops_closed`;
  loadgen telinde `GSNB`, `RESULT`'ta (savaş) iki anahtar, her zaman
  basılır. Kapalı sayısının motorun garanti ettiği bir üst sınırı yok:
  shard'ın `Shutdown`'ı yerinde teslim edildiyse shard başına en çok bir,
  gelen kutusu doluysa `Shutdown` sonra gelir ve shard bir kez daha
  adımlayabilir.
- **Registry kapsamı: kontrol düzlemi kayıpları (B57).** Registry
  satırında `rooms_died=`'den sonra dört anahtar ve aile tablosunda
  (`REGISTRY`) dört `counter`:
  `join_ops_dropped=` / `gsb_registry_join_ops_dropped_total` — registry'nin
  bağlantının op dağıtıcısına veremediği katılmalar (16'lık kuyruk dolu ya
  da görev gitmiş; istemci `ERROR` "registry unavailable" alır);
  `close_ops_dropped=` / `gsb_registry_close_ops_dropped_total` — kapanan
  bağlantının `Close` op'u aynı yolda düştü. Üyelik yine biter (B61):
  kuyruğu dolu dağıtıcı önündeki op'ları sırayla işleyip kuyruğu
  kapanınca detach eder, gitmiş dağıtıcının yerine registry tablodaki
  üyeliği doğrudan detach eder — sayaç "op kuyruğa girmedi" demektir,
  sızıntı değil;
  `match_results_dropped_full=` / `…_closed=`
  (`gsb_registry_match_results_dropped_{full,closed}_total`) — duran
  oda/shard'ın maç sonucunu sonuç sink'i reddetti: DOLU (tüketici okumuyor)
  ya da KAPALI (tüketici alıcısını bıraktı). Duran oda başka örnek
  göndermediğinden sonuç kaybı toplayıcıya kendi olayıyla gider
  (`MetricsEvent::MatchResultDropped`) ve registry diliminde raporlanır
  (registry dilimi yokken — registry hiç örnek göndermeden — görünmez;
  sunucuda oda registry'den önce var olamaz). B62'den beri olay
  durdurma-mesajı deyimiyle gider (`channel::post`): dolu metrik
  kanalında düşmez. Toplayıcı da gitmişse (süreç kapanırken) sayılamaz.
  Duran odanın son örneği (`MetricsEvent::RoomFinal`, B62) yeni aile
  getirmez: odanın satırına düşer (yok edilen odanın bekleme penceresinde
  de); `gsb_room_requests_undelivered_total` ve
  `gsb_room_requests_abandoned_total` HELP'leri odanın duruşunu da
  oturum sonları arasında sayar (altın metinler bu iki HELP kadar). Loadgen telinde `GSMP` (registry
  bölümünde `closes`'tan sonra); `RESULT`'ta yok (satır registry
  sayaçlarını taşımıyor).
  **B57'nin "sayılmayan, bilerek" kararı F56'da değişti:** odanın
  registry'ye kapatma/ayrılma isteği ve detach-despawn raporu
  registry'nin posta kutusu KAPALIYKEN düşer — kutu yalnız registry
  `Shutdown` kolunda kapanır. B57 bunu "çağlayan zaten yapar" diye
  saymıyordu; ama hüküm kaybolur (istemci `ERROR 9` yerine `ERROR 14`
  alır, `server_closes` gerekçeyi yazmaz). Artık sayılıyor: aşağıda
  "duruşun yuttuğu hükümler (F56)".
- **Oda kapsamı: duran odanın/shard'ın oturum dışı kalanları (B68, sayım
  turu 4).** Oda satırında `metrics_dropped=`'den sonra (mantık
  sayaçlarından önce) dokuz anahtar, aile tablosunda oturum ailelerinden
  sonra dokuz `counter` (`metrics/export/families/stop.rs`,
  `gsb_room_<ad>_total`); loadgen telinde `GSMV` (oda başına
  `metrics_dropped`'tan sonra), `RESULT`'ta aynı adlarla (her satırda),
  fold'da SUM. Kaynak `metrics::StopCounts`; yalnız SON örnekte dolu
  (`RoomFinal`), periyodik örneklerde 0. Duruş kanalı kapatır ve kalanı
  sayar: `joins_unprocessed` — kuyrukta kalan katılma (dağıtıcı
  bağlantıya `RoomGone` yanıtlar); `resumes_unprocessed` — kuyrukta kalan
  resume (shard'da yalnız kimliği park etmiş shard'da);
  `leaves_unprocessed` / `detaches_unprocessed` — burada etki edecek
  ayrılma / taşıma ölümü (bayatlar sayılmaz; `on_disconnect` hiç
  koşmadı). Bu ikisi duruşun DEFTERİDİR (F55): kanal kapandıktan sonra
  varan ayrılma/taşıma ölümü dağıtıcıda reddedilir ve hiçbir yerde
  sayılmaz — üyeyi duruş zaten bitirdi, oturumunun elindeki B62 ile
  sayıldı; hangisinin kanala önce girdiği zamanlamaya bağlıdır, iki
  duruşu bu sayılarla kıyaslamayın; yalnız shard'da: `migrations_in_dropped` (gelen göç,
  kurulmadı; göç eden oyuncunun okunmamış girdisi
  `requests_dropped_unread`/`actions_dropped_unread`'e),
  `effects_unsent` (yeniden deneme tamponu + tick'in giden kuyruğu),
  `effects_unapplied` (vadesini bekleyen + kutudaki), `team_imports_unapplied`,
  `border_updates_unapplied` (son ikisi görünüm kopyası: kaynak hâlâ
  tutar). **Anlam notu:** yayın op'ları shard sayısı kadar şişmez —
  etki edeceği tek shard sayar. `migrations_out` = kurulan
  (`migrations_in`) + `migrations_in_dropped` (+ göndericide
  `migrations_failed` olanlar zaten `migrations_out`'ta değil).
- **Registry kapsamı: son sayımı olmadan biten görevler (B67, sayım
  turu 4).** Registry satırının sonunda `rooms_ended_uncounted=` ve aile
  tablosunda `gsb_registry_rooms_ended_uncounted_total` (`counter`);
  loadgen telinde `GSMU` (registry bölümünde
  `match_results_dropped_closed`'dan sonra); `RESULT`'ta yok. Panikleyen
  (ya da iptal edilen) oda/shard görevi `finish`'e varmaz, son örneğini
  (`RoomFinal`) göndermez: son penceresi ve elinde kalanlar bilinemez.
  Görevin ölüm bekçisi bunu `MetricsEvent::RoomEndedUncounted` ile görevin
  SATIR kimliğiyle bildirir; toplayıcı sayar ve satırın bekleme penceresini
  başlatır (ölen shard'ın satırı artık budanıyor). **Anlam notu:**
  `rooms_died` biçilen MANTIKSAL odaları sayar, bu sayaç son sayımsız
  biten GÖREVLERİ — sharded odada ölen her shard bir; hayatta kalan
  shard'lar artık biçilen odayla durur ve son sayımlarını verir (önceden
  sunucu durana dek çalışıyorlardı). Kaybolan sayıların kendisi
  bilinemez; sayaç kaybın VAR olduğunu söyler.
- **Registry kapsamı: takım hub'ının reddedilen röleleri (B72, sayım
  turu 5).** Registry satırının sonunda (`rooms_ended_uncounted=`'den
  sonra) `team_relays_dropped_full=` / `team_relays_dropped_closed=` ve
  aile tablosunda (`REGISTRY`) `gsb_registry_team_relays_dropped_full_total`
  / `…_closed_total` (`counter`); loadgen telinde `GSMW` (registry
  bölümünde `rooms_ended_uncounted`'dan sonra); `RESULT`'ta yok (satır
  registry sayaçlarını taşımıyor). Sharded odanın takım hub'ı
  (CROSS-SHARD §8b.2; registry'nin içinde) bir export'u hedef shard'lara
  `try_send` ile röleler; reddedilen import önceden yalnız
  `team_hub_summary` log satırında, DOLU ve KAPALI birlikte
  (`relay_drops`) sayılıyordu. Artık sebebe göre ayrı: **dolu** — hedef
  shard yetişemiyor; hedefin kaynağa bağlılığı durur, kaynağın sonraki
  export'u bütün kümeyi yeniden taşır (gecikme; kalıcı kayıp değil);
  **kapalı** — hedef shard durmuş ya da ölmüş, okuyan yok. Sayı sebebin
  yaşadığı yerde tutulur: hub oda kaydıyla birlikte gider, bu yüzden
  `on_export` o export'un reddettiklerini döndürür, registry kümülatif
  toplamı kendisi tutar (`reg_team_relays_dropped_{full,closed}`) ve bir
  ret olduğunda örneğini HEMEN gönderir — registry yalnız bir tablo
  değişince örnek yollar, röle tablo değiştirmez (ret yoksa ek örnek yok;
  maliyet ret içeren export başına bir `try_send`). Log satırının
  penceresi de ikiye ayrıldı (`relay_drops_full`, `relay_drops_closed`).
  Shard tarafının eşi (export'u registry kutusu reddetti) F50'de aynı
  biçimde sebebe göre ayrıldı: `gsb_room_team_export_drops_{full,closed}_total`.
- **Registry kapsamı: duran odanın reddettiği katılmalar (B75, sayım
  turu 6).** Registry satırının sonunda (`team_relays_dropped_closed=`'den
  sonra) `joins_refused_closed=` ve aile tablosunda (`REGISTRY`)
  `gsb_registry_joins_refused_closed_total` (`counter`); loadgen telinde
  `GSMY` (registry bölümünde `team_relays_dropped_closed`'dan sonra);
  `RESULT`'ta yok. Bağlantının dağıtıcısı bir katılmayı (anonim ya da
  kimlikli — resume denemesi) odaya gönderir; oda/shard durmuş ya da
  ölmüşse kutusu KAPALIDIR, gönderim reddedilir, istemci `RoomGone` alır
  — oda op'u hiç görmedi, hiçbir oda saymaz. Artık dağıtıcı sayar:
  `MetricsEvent::JoinRefusedClosed` (durdurma-mesajı deyimi,
  `channel::post`) toplayıcıya DOĞRUDAN gider — registry üzerinden değil,
  çünkü bütün sunucunun duruşunda registry dağıtıcılardan önce çıkar.
  **Anlam notu:** oda op'u alıp duruşta kuyrukta bırakırsa o kayıp odanın
  `joins_unprocessed`/`resumes_unprocessed`'idir, bu sayaç değil — her
  katılma tam bir yerde. Sharded resume yayını: kimliği kendi parkında
  olan duran shard resume'u sayar ve `RoomGone` der (katlama taze join'e
  düşmez); parkı hiçbir shard'da olmayan resume taze join'e düşer ve orada
  bir kez sayılır (ev shard'ı kapalıysa burada, açıksa ev shard'ının
  `joins_unprocessed`'inde). Reddedilen yayın gönderimleri (shard başına)
  sayılmaz — op başına bir karar (RECONNECT §3.4, §6).
- **Registry kapsamı: registry'nin duruşta okumadığı kutu (F53).**
  Registry satırının sonunda (`joins_refused_closed=`'den sonra)
  `joins_unread=` / `team_exports_unread=` ve aile tablosunda (`REGISTRY`)
  `gsb_registry_joins_unread_total` /
  `gsb_registry_team_exports_unread_total` (`counter`); loadgen telinde
  `GSNC` (registry bölümünde `joins_refused_closed`'dan sonra);
  `RESULT`'ta yok. Registry `Shutdown`'dan sonra hiçbir şey okumaz;
  önceden alıcıyı arkasına kuyruklanmış mesajlarla birlikte düşürüyordu
  (her göndericinin gönderimi BAŞARILI dönmüştü). Artık `Shutdown` kolu
  kutuyu KAPATIR (sonraki gönderim göndericide reddedilir, sayıyorsa
  orada sayılır: shard'ın export'u `team_export_drops_closed`),
  `try_recv` ile boşaltır (kapalı kutu sonludur) ve türe göre karar
  verir: canlı enkarnasyonun takım export'u — shard onu `team_exports`'ta
  "kuyruklandı" saymıştı, hub hiç rölelemedi → `team_exports_unread`;
  katılma (`SpawnPlayer`, resume denemeleri dahil) — hiç işlenmedi,
  yanıtı düşer, istemci `ERROR` "registry unavailable" alır →
  `joins_unread`; duruşun arkasında açılan bağlantı (`ConnOpened`;
  sunucunun kendi `stop()`'u F41'den beri kapıları önce kapatır)
  kayıtlılar gibi `ConnIn::Shutdown` alır — kayıp yok, sayaç yok. Gerisi
  sayılmaz, çünkü duruşun kendisi onu yapar: kopuş (`ConnClosed`) ve
  ayrılış (`DespawnPlayer`) — teardown'un `RoomOp::Close`'u her dağıtıcıya
  tuttuğu üyeliği detach ettirir, her oda durur; dağıtıcı yankıları,
  `RoomDied` (panik bekçide sayılır, B67), `Authed` — teardown'un
  düşürdüğü tabloları günceller; odanın hükümleri (`CloseConn`,
  `LeaveConn`, `DetachDespawned`) F53'te sayılmıyordu — F56'dan beri
  kayıp hüküm olarak sayılıyor (aşağıda);
  kontrol düzlemi istekleri — düşen yanıt çağırana hatadır. Sayılar
  registry'nin SON örneğinde: `Shutdown` kolu teardown'dan önce onu
  `channel::post` ile gönderir (dolu kanalda spawn'lu gönderici kendi
  klonunu tutar); toplayıcının son raporu her oturum üreticisinin
  göndericisini düşürmesini beklediğinden (F35) o örnek son rapordadır.
  Periyodik örneklerde ikisi de 0. Tablo göstergeleri (`rooms`, `conns`)
  son örnekte teardown'dan ÖNCEKİ değerlerdir (önceki örneklerle aynı
  anlam).
- **Registry kapsamı: duruşun yuttuğu hükümler (F56; B57'nin kararı
  yeniden).** Registry satırının sonunda (`joins_unsent=`'ten sonra)
  `close_verdicts_lost=<toplam>`, gerekçe başına bir
  `close_verdict_lost_<gerekçe>=` (sıfırlar dahil, net satırının
  `server_close_<gerekçe>=` yazımı), `leave_verdicts_lost=`,
  `detach_despawns_lost=`; aile tablosunda
  `gsb_registry_leave_verdicts_lost_total`,
  `gsb_registry_detach_despawns_lost_total` ve etiketli
  `gsb_registry_close_verdicts_lost_total{reason}` (`ServerClose`
  kümesi, `gsb_net_server_closes_total` ile aynı etiketler); loadgen
  telinde `GSNE` (registry bölümünde `joins_unsent`'ten sonra: 13
  gerekçe, sonra ikisi); `RESULT`'ta yok. Odanın/shard'ın registry'ye
  verdiği hüküm — kapatma (`CloseConn`: oyunun atması, girdi-boşta
  tavanının `disconnect`'i), ayrılma (`LeaveConn`: tavanın varsayılan
  `leave_room`'u), detach-despawn raporu (`DetachDespawned`) — duruşta
  şu yerlerden TAM BİRİNDE yakalanır ve orada sayılır: odanın
  kuyruğunda (registry kutusu doluydu) duruşa kalmış → odanın/shard'ın
  `finish`'i; registry'nin KAPALI kutusu reddetti → odanın flush'ı;
  registry kutusunda `Shutdown`'ın arkasında okunmadı → registry'nin
  boşaltması (F53); işlendi ama bağlantının kutusunda duruşun
  `ConnIn::Shutdown`'ının ARKASINDA kaldı → bağlantı aktörünün sonu (bu
  ayak her sunucu hükmü için: pompanınki, yok edilen odanınki de; oturum
  başına yalnız ilki). Her yer saydığını tek
  `MetricsEvent::VerdictsLost` ile (`channel::post`) gönderir, toplayıcı
  registry diliminde toplar. Kapatma hükmü `server_closes`'un hiç
  yazmadığı aynı gerekçeyle sayılır: `server_closes{r}` +
  `close_verdicts_lost{r}` = kararı verilen oturum sonu. Hükmün etkisi
  boş olacak olsa da (bağlantısı zaten gitmiş) sayılır — sayaç hükmü
  sayar; odanın kapalı-kutu reddi tabloya bakamaz, registry de aynı
  ölçütle sayar. Registry'nin bağlantıya her bildirimi (F57) yerinde
  kuyruklanır (`channel::post`): kutuda yer varsa duruşun
  `Shutdown`'ının önündedir. **Kalan:** bağlantının kutusu doluyken
  işlenen hüküm spawn'lu yedekle gider; bağlantı kutusunu kapattıktan
  sonra varırsa reddedilir, sayılmaz.
- **Registry kapsamı: kapalı registry'nin reddettiği katılmalar (F54).**
  Registry satırının sonunda (`team_exports_unread=`'den sonra)
  `joins_unsent=` ve aile tablosunda (`REGISTRY`)
  `gsb_registry_joins_unsent_total` (`counter`); loadgen telinde `GSND`
  (registry bölümünde `team_exports_unread`'den sonra); `RESULT`'ta yok.
  Registry `Shutdown`'da kutusunu kapatır (F53); ondan SONRA hâlâ yaşayan
  bir bağlantının JOIN'i (anonim ya da resume denemesi) bağlantı
  aktöründe reddedilir, istemci `ERROR` "registry gone" alır. Bağlantı
  aktörü `MetricsEvent::JoinUnsent`'i `channel::post` ile gönderir,
  toplayıcı registry diliminde sayar. **Anlam notu:** `joins_unread`'in
  kapalı eşi — kutuya girmiş ama okunmamış katılma oradadır, bu sayaçta
  değil; her katılma tam bir yerde.
- `/rooms` çıktısı da insan-okunur düz metin (JSON yok kararıyla tutarlı);
  makine-okunurluk için ileride gerekirse ayrı karar
- HTTP task'inin tek await'i accept `recv`; bağlantı başına kısa ömürlü
  task (istek başına tam okuma + tek yanıt + kapanış) — aktör disiplini
  bozulmaz, select gerekmez
- **İstek başlığının süre sınırı (B47).** Bağlantı istek başlığının
  TAMAMINI (istek satırı + başlıklar, CRLFCRLF'e dek) `HEAD_DEADLINE` =
  **5 sn** içinde göndermeli (`http/head.rs`; config'siz sabit, TLS
  el sıkışma süre sınırıyla aynı aile — SECURITY §2 "Uçlar"). Aşılırsa
  tek bir `408 Request Timeout` (`Connection: close`, kısa gövde) yazılır,
  ardından normal kapanış yolu: `shutdown` + 300 ms'lik sınırlı boşaltma
  (`DRAIN_WINDOW`), görev biter. Eskiden bağlanıp tek bayt göndermeyen
  bir eş bağlantı görevini kopana dek tutuyordu (slowloris türü sızıntı;
  `stop`'u tutmuyordu — görev accept döngüsünden ayrı, B33). Kararlar:
  (1) süre **başlığın tamamına** — okuma başına değil: bayt bayt damlatan
  eş de aynı anda kesilir; (2) tek `tokio::time::timeout` okumanın
  etrafında — okuma tek beklenen şey, çoklu bekleme yok; (3) **408
  yazılır** (sessiz kapatma değil): RFC 9110'un boşta bağlantı yanıtı,
  küçük ve tek, tam okunmamış başlık gönderen dürüst bir istemciye neden
  kesildiğini söyler; yazma yolu 400/431'inkiyle aynı; (4) 5 sn: bir
  scraper ya da `curl` başlığını bağlanır bağlanmaz tek yazmada yollar —
  yavaş bir hatta bile dürüst istemcinin çok ötesi, sessiz görevin de
  kısa sürede ölmesine yeter.
- **Bağlantı tavanı ve yanıt yazmanın süre sınırı (B49).** B47 yalnız
  başlık okumasını sınırlamıştı: başlığını gönderip yanıtı hiç okumayan
  bir eş, soket tamponlarından büyük bir yanıtta (çok odalı `/metrics`)
  görevini bağlı kaldıkça tutuyordu ve bir eşin açabileceği görev
  sayısına sınır yoktu. İki sunucu anahtarı (varsayılan AÇIK — ops
  yüzeyi localhost sözleşmesi, değerler cömert):
  ```toml
  http_max_connections = 64      # vars. 64; 0 = tavan yok
  http_write_timeout_secs = 10.0 # vars. 10 sn; 0 = sınır yok
  ```
  (1) **Tavan:** accept döngüsü canlı bağlantı görevi başına bir yuva
  tutar; tavandaki yeni bağlantı hemen, okunmadan ve yanıtsız kapanır
  (ret yolunda yazma yok — yavaş okuyan eşe yazmak iş olurdu) ve sayılır
  (`ops_http_conns_refused`); doymuş dönemin ilk reddi tek `warn`.
  Gerekçe: yüzeyin çağıranları bir-iki scraper, sağlık yoklaması ve
  operatörün `curl`'u — bir avuç eşzamanlı bağlantı; 64 büyüklük
  mertebesi pay ve bir eşin tutabileceği görev (ve yanıt arabelleği)
  sayısına sınır. (2) **Yazma süre sınırı:** yanıtın TAMAMI (yazma +
  ardından yarı kapanış) tek `timeout` altında — B47'nin başlık okuması
  gibi, yazma başına değil; aşılırsa bağlantı düşer (kapanır) ve sayılır
  (`ops_http_writes_timed_out`, her biri `warn`). Gerekçe: çok megabaytlık
  bir `/metrics` loopback ya da LAN'da milisaniyede, yavaş bir yönetim
  hattında saniyelerde geçer; 10 sn oyun kapılarının yazma tıkanması
  varsayılanı (`write_stall_secs`). Okuyan ama yavaş eş tam yanıtı alır.
  Böylece bir bağlantı görevi en çok 5 sn (başlık) + yönlendirme + 10 sn
  (yazma) + 300 ms (boşaltma) yaşar ve aynı anda en çok 64 tanesi.
  `0` (ya da negatif / sonsuz süre — `write_stall_secs` kuralı) ilgili
  sınırı kapatır. Katman yok: `[rooms.<id>]` reddeder. *Sınır dışı
  kalan:* yönlendirmenin kendisi (`/rooms`, oda açma/kapama registry
  yanıtını bekler) yazma süresine dahil değil; registry'nin cevabı
  kendi sınırında.
- Kapanış (B33): accept, oyun dinleyicileriyle aynı `Door`'dan geçer
  (B16). `ServerHandle::stop` kapıyı kapatır, bekleyen accept
  `listener_closed` ile biter, döngü döner ve listener'ı düşürür (port
  kapanır); `stop` döngüyü oyun dinleyicilerininkilerle aynı 1 sn'lik
  son tarih altında bekler, abort yalnız geri sigortadır. `StopReport`
  onu accept döngülerinden biri olarak sayar (`accept_loops_ended`,
  `http_listen` açıkken +1). Kabul edilmiş bağlantılar kendi
  görevlerinde yanıtlarını bitirir (DESIGN §9)
- **Dışa açım yüzeyleri (E2, §6).** Aynı katlanmış rapor üç yoldan
  çıkar: `gsb-metric` log satırı (`MetricSink::Log`), `/metrics`
  (Prometheus, çekme; `prometheus` feature'ı, varsayılan açık) ve
  `[metrics.otlp]` (OTLP/HTTP protobuf, itme; `otlp` feature'ı,
  varsayılan kapalı). Prometheus ile OTLP **tek aile tablosunu** yürür
  (`gsb_core::metrics::export::families`: ad, tür, help, okuyucu, sıra):
  yeni bir aile iki yüzeye birden girer, biri öbüründen kopamaz. OTLP
  eşlemesi:

  | Bizdeki aile | OTLP |
  |---|---|
  | `counter` (kümülatif) | `Sum`, monotonic, CUMULATIVE, `as_int`; `start_time` = exporter'ın doğumu |
  | `gauge` (registry: `rooms`, `conns`) | `Gauge`, `as_int` |
  | `gauge` (oda: `hz`, ortalamalar, uçlar, oranlar, sayımlar) | `Gauge`, `as_double` |
  | `gsb_room_step_hist` (log2, bütçe oranları) | `Histogram`, CUMULATIVE; sınırlar odanın bütçe kenarları (µs, `le` ile aynı), 14 kova; `sum` = ortalama × adım, `min`/`max` = `step_min_us`/`step_max_us` |
  | `gsb_room_step_duration_us` (Prometheus'ta p50/p99 `summary`) | `Histogram`, CUMULATIVE; sınırlar 8, 16, …, 4096 µs (512), 513 kova — sonuncusu ince tavanın üstündeki adımlar (log2 nüfusunun kalanı), yani dağılım kayıpsız |
  | `gsb_net_server_closes_total{reason}` | `Sum`, `reason` özniteliği, sıfırlar dahil |
  | mantık sayacı SUM / MAX (F9) | `Sum` monotonic / `Gauge`, `as_int`; açıklama bildirimin help'i |
  | `gsb_room_logic_counters_dropped` (F17) | `Gauge`, yalnız sıfırdan büyükken |
  | — (yalnız OTLP) | `gsb_export_otlp_reports_dropped`, `gsb_export_otlp_push_failures`: exporter'ın kendi sağlığı, `Sum` |

  Ad kuralı: OTLP adı Prometheus ailesinin adından `_total` düşmüş
  hâlidir (`gsb_room_steps`); bir collector'ın Prometheus exporter'ı
  monotonic sum'a `_total`'ı geri ekler, yani iki yol aynı seri adında
  buluşur. Oda `room="r<id>"` özniteliğidir (etiketin değeri), resource'ta
  `service.name` (config), scope `gsb` + crate sürümü. `unit` alanı
  boştur: birim adın içinde (`_us`, `_bytes`) — alan doldurulsa aynı
  exporter birimi ada ikinci kez eklerdi. Kova sınırı yaklaşıklığı
  Prometheus `le`'siyle aynıdır (bizim binler alt-kapalı `[lo, hi)`,
  OTLP'ninki üst-kapalı). Prometheus gibi OTLP de **katlamaz**: shard
  başına bir nokta (DESIGN §12 "tutarlı kesit"). `actions_dropped_top`
  ikisinde de yok (log satırında). İki yüzeyin aynı şeyi söylediğinin
  kilidi `metrics::tests::otlp::cross` (aile kümesi, sırası, türü,
  açıklaması, değerleri; histogramın kümülatif kovaları, kenarları,
  `count`/`sum`'u; summary'nin p50/p99'u OTLP ince kovalarından yeniden
  türetilir).

## 4. Test planı

1. `healthz_reports_ok_while_ticker_runs` / liveness 503 dalı
2. `metrics_endpoint_exposes_known_counters` — bilinen bir sayacı
   artıran senaryo + scrape'ta görünürlük
3. `admin_open_status_close_round_trip` — runtime oda yaşam döngüsü
   ve `http_room_config::admin_open_uses_the_server_room_config` — oda anahtarları
   varsayılandan farklı sunucuda ön-kurulan odayı yeniden açmak 200,
   başka hız 409, yeni oda + tekrar 200, geçersiz hız 400 (F8);
   yüzeyin kendi birim testleri (`http/tests.rs`): registry'ye giden
   istek `Config::room_config(id)`'nin alanı alanına aynısı, `tick_hz`
   tek geçersiz kılma, geçersiz hız registry'ye hiç gitmez; uçtan uca
   `mmo_rooms::a_runtime_room_gets_the_server_ceiling`. Oda başına
   override (B18): `http::tests::the_open_takes_the_id_override_with_the_query_rate_on_top`
   (id'nin override'ı, override'sız id sunucunun odası, query hızı
   override'ın üstünde); `room_overrides::the_admin_open_builds_the_overridden_room`
   (override'lı başlangıç odasını yeniden açmak kendi hızıyla 200,
   sunucunun hızıyla 409; runtime id 200 + tekrar 200, registry'deki oda
   `room_config(7)`'nin aynısı); `room_overrides::a_room_cap_of_its_own_refuses_the_third_join`
   (`max_players = 2`'li oda üçüncü katılımı RoomFull ile reddeder, aynı
   bağlantıyı sunucunun diğer odası kabul eder); ayrıştırma/doğrulama
   `config/axes/listeners/room/{tests,overrides/tests}.rs` ve
   `room_overrides::a_room_the_registry_would_refuse_refuses_startup`
4. `disabled_by_default_and_binds_when_configured` — varsayılan kapalı,
   config'li çalışma
5. Prometheus render fonksiyonunun unit testleri (HTTP'den bağımsız)
6. Başlık süre sınırı (B47, `http/tests/head_deadline.rs`, paused saat,
   bellek içi boru üstünde bağlantı görevi): sessiz eş tam `HEAD_DEADLINE`'da
   (öncesinde değil) `408` + `Connection: close` alır, görevi boşaltmadan
   sonra biter, borunun ucu kapanır; saniyede bir bayt damlatan eş aynı
   anda kesilir (süre başlığın tamamına); başlığı hemen ya da sınırdan
   100 ms önce gelen `/metrics` isteği anında normal yanıtını alır. Önce
   kırmızı (süre sınırı yokken iki test asılma korumasına takıldı);
   öldürülen mutasyonlar: süre sınırı yok, okuma başına süre, zaman
   aşımına 400, zaman aşımında yanıtsız kapatma, sınırın 100'de biri
6b. Bağlantı tavanı ve yazma süre sınırı (B49, `http/tests/limits.rs`):
   paused saatte 64 baytlık boru üstünde isteğini yollayıp hiç okumayan
   eş tam `http_write_timeout_secs`'te (öncesinde değil) kesilir, sayılır,
   görevi biter; okuyan yavaş eş yanıtın tamamını alır, sayaç 0; gerçek
   soketlerle tavan 2'de iki sessiz eş varken üçüncü bağlantı hemen
   yanıtsız kapanır ve sayılır, bırakılan yuva sonrakine hizmet eder,
   kapı kapanınca ret toplayıcıya ulaşır; anahtarların çözümü (vars. 64 /
   10 sn; 0, negatif, sonsuz, NaN = kapalı). `tests/ops_limits.rs`:
   varsayılanlar, ayrıştırma, `[rooms.<id>]` reddi; sunucu arkasında
   tavan üstü bağlantı reddi yüzeyin kendi `/metrics`'inde
   `gsb_transport_ops_http_conns_refused_total` olarak görünür. Önce
   kırmızı (süre sınırı ve tavan yokken); öldürülen mutasyonlar raporda.
7. Başlatma hatası (F63, `tests/startup_errors.rs`): gerçek ikililer
   (`CARGO_BIN_EXE_gsb-server`, `…gsb-loadgen`) bir şey bağlamadan
   reddeden config'lerle koşar — girdide bilinmeyen anahtar (anahtar,
   `line 8`, alıntılanan satır; dosyanın başka satırı yok), aralık dışı
   ve negatif `listen_backlog`, yüklenemeyen TLS dosyası (kapı + dosya),
   dolu port (adres), okunamayan config yolu, süreç-içi loadgen reddi;
   hepsinde çıkış durumu 1, stderr'de `Error: ` / `Parse {` /
   `input: Some(` yok. Birim: `error_chain::tests` (metnin taşıdığı neden
   tekrar edilmez, taşımadığı ayrı satır; ayrıştırma hatası = `Display`,
   sonda boş satır yok), `config_debug::tests` (`Debug`'da dosya yok).
   Önce kırmızı (yedisi de); öldürülen mutasyonlar: `Debug` basmak,
   çıkış durumu 2, zincirde her nedeni eklemek / hiçbirini eklememek,
   kırpmamak, TLS kapısına `tcp` demek, `Debug`'da toml hatasının
   kendisi, loadgen'de panik

## 5. NOT-DONE (v1)

- Auth/TLS (localhost sözleşmesi ile yaşar), keep-alive/chunked,
  JSON/protobuf çıktı, /debug/pprof tarzı profillendirme, ops HTTP için
  çoklu-listener (admin/metrics sunucusu tek `http_listen` adresinde
  dinler). *Not: oyun taşımalarının çoklu-listener'ı (`[[listeners]]`:
  tcp/tls/udp/quic/ws) ayrı bir iştir ve yapıldı — CHANGELOG
  "çoklu-listener'a QUIC + WS kapıları turu", ROADMAP devam notu; bu
  madde yalnız ops HTTP'yi kasteder.*

## 6. Dışa açım katmanı: takılabilir exporter'lar (BACKLOG E2)

**Karar (bakımcı, 2026-09-27):** içeride ucuz toplama aynı kalır
(aktör-yerel sayaçlar → sınırlı kanal → tek toplayıcı → katlanmış
`MetricReport`); dışa açım tek bir yerde, takılabilir exporter'lara
devredilir — Prometheus mevcut, OTLP eklendi, gerekirse `metrics`
fasadı — her biri feature arkasında. Bu bölüm, eski "Kenara not"un
(`metrics` fasadı + `metrics-exporter-prometheus` dış önerisi, o gün
uygulanmadı; "dördüncü lavabo" yönü) yerini alan tasarımdır.

| # | Karar | Gerekçe / elenen |
|---|---|---|
| 1 | **Dikiş: `Exporter` trait'i** (`gsb_core::metrics::Exporter`, `fn export(&mut self, &MetricReport)`). Toplayıcının `emit`'i TEK dışa açım yeridir: her rapor önce kurulu exporter'lara sırayla (salt-okunur), sonra `MetricSink`'e (raporu tüketir) gider; kapanıştaki son rapor dahil. Kurulum `MetricsCollector::with_exporters`; `FnMut(&MetricReport)` kapanışları da exporter'dır | Exporter saf tüketicidir: toplayıcıya ya da aktörlere uzanacak tutamağı yoktur, hiçbir aktör kaç exporter olduğunu bilmez — aktör kodu değişmedi. Elenen: `MetricSink`'e dördüncü varyant — sink TEK hedeftir (ops yüzeyi açıkken `Watch` onun yerini alır), OTLP ise log/kanal/watch'un hangisiyle olursa olsun yan yana yaşamalı |
| 2 | **Exporter bloklamaz.** `export` toplayıcının görevinde, senkron çağrılır; G/Ç yapan bir exporter raporu kendi görevine **sınırlı devirle** verir | Toplayıcı tek-await'lidir ve her aktörün örneklerini boşaltır; yavaş bir exporter hepsini bekletirdi |
| 3 | **Çekme vs itme.** Prometheus (çekme): ops yüzeyinin `watch` anlık görüntüsünden **kazıma anında** render (`MetricReport::render_prometheus`) — kimse kazımıyorsa render yok, `/metrics` baytları değişmedi (`metrics::tests::golden`). OTLP (itme): bir `Exporter` | Elenen: Prometheus metnini her raporda önceden render edip ikinci bir `watch`'a koymak — kazıyıcı yokken her saniye boşa render, `/healthz` yine rapor watch'unu isterdi |
| 4 | **Tek aile tablosu** (`metrics::export::families`: ad, tür, help, okuyucu, sıra; kapsamlar `METRICS_DROPPED`, `REGISTRY`, `NET`, server-closes, `TRANSPORT` — B58 —, oda aileleri); Prometheus render'ı ve OTLP eşlemesi aynı tabloyu yürür | Yeni sayaç iki yüzeye birden girer; ad/tür kayması yapısal olarak imkânsız. Tablo taşınırken Prometheus metni bayt bayt aynı kaldı (altın test; help/tür/sıra mutasyonları onu kırar). Biçime özgü kalanlar: dağılımların şekli, mantık sayaçlarının (ad kümesi rapor anında belli) biçimi |
| 5 | **Feature düzeni `gsb-core`'da:** `prometheus` (varsayılan açık) render'ı ve aile tablosunu, `otlp` (varsayılan kapalı) OTLP exporter'ını derler. Workspace bağımlılığı `gsb-core`'u **varsayılan feature'sız** alır; hangi exporter'ın var olduğuna yalnız `gsb-server` karar verir (`prometheus` varsayılan, `otlp` = `gsb-core/otlp`). `gsb-core`'un kendi test koşusu varsayılanını (Prometheus) tutar | Neden yeni bir `gsb-export` crate'i değil: OTLP **hiç yeni bağımlılık getirmiyor** (aşağıda) — "opsiyonel bağımlılıkları çekirdekten uzak tut" gerekçesi boşa düşer; render `MetricReport`'un inherent metodu ve altın test çekirdekte; ayrı crate aynı exporter ailesini iki crate'e bölerdi |
| 6 | **Yeni bağımlılık YOK.** OTLP mesajları `opentelemetry-proto` v1'in elle yazılmış bir ALT KÜMESİ (`prost` derive, üst akış alan numaraları); istek düz tokio `TcpStream` üstünde tek bir HTTP/1.1 POST | Elenen: `opentelemetry` + `opentelemetry-otlp` (SDK ağacı, global meter provider), `opentelemetry-proto` + `prost-build` (kod üretim adımı, aynı baytlar), `tonic`/gRPC (HTTP/2 yığını), `hyper`/`reqwest` (tek istek için istemci yığını). Alan numaraları iki yoldan doğrulandı: derive'dan bağımsız bir protobuf yürüyücüsüyle (`otlp::tests::wire`) ve bir kez sistem `protoc`'u + resmî `opentelemetry-proto` dosyalarıyla (`--decode`: bilinmeyen alan yok) |
| 7 | **OTLP/HTTP protobuf**, `POST <endpoint>` (`Content-Type: application/x-protobuf`, `Connection: close`); yol boşsa `/v1/metrics`. **Yalnız `http://`** — `https://` başlatmayı durdurur | gRPC'den hafif: aynı mesaj, HTTP/2 yok. TLS: hedef sunucunun yanındaki collector/agent'tır, TLS'i ötesine o taşır (ops HTTP'nin localhost sözleşmesiyle aynı çizgi) |
| 8 | **Geri basınç: tek yuvalı devir + düşür-ve-say.** Vadesi gelen rapor `try_reserve` ile 1 yuvalı kanala klonlanır; yuva doluysa (itme görevi hâlâ öncekinde) rapor düşer, sayılır ve sayı bir sonraki devirle `gsb_export_otlp_reports_dropped` olarak gider. İtme hatası (bağlantı, zaman aşımı, 2xx olmayan durum) sayılır (`gsb_export_otlp_push_failures`, sonraki itmede), **yeniden denenmez**, kesinti başına bir `warn` + düzelişte bir `info` | Her değer kümülatif ya da gauge: düşen/başarısız rapor, sonrakinin taşıdığından fazlasını taşımaz (örnek kanalıyla aynı mantık). Daha derin kuyruk yalnız bir sonraki vadeli rapordan ESKİ raporları tutardı. Elenen: yeniden deneme kuyruğu, `watch` ile "en yeni kazanır" (üzerine yazılan okunmamış raporu saymanın yolu yok) |
| 9 | **Kadans: sabit ızgara.** İlk rapor iter; sonraki vade bir öncekinin vadesine `interval` eklenerek ilerler (rapor zamanına değil) — toplayıcının titremesi aralığı uzatmaz. İtme zaman aşımı = bir aralık | Rapor anına göre vade, 1 sn'lik raporlarla 10 sn'lik aralığı ~11 sn'ye kaydırırdı |
| 10 | **Config:** `[metrics.otlp]` `endpoint` (zorunlu), `interval_secs` (vars. 10, `0` başlatmayı durdurur), `service_name` (vars. `"gsb"`); bilinmeyen anahtar ayrıştırmayı durdurur. Tablo her derlemede AYRIŞIR: `otlp`'siz derlemede başlatma `ServerError::OtlpNotBuilt` ile adıyla durur; geçersiz hedef `ServerError::BadOtlp`. Hepsi bir şey bağlanmadan önce | Operatörün istediği itme asla sessizce atlanmaz |
| 11 | `prometheus`'suz derlemede `/metrics` **404 + feature adı** (yol bilinir: başka fiil 405) | Boş bir 200'ü kazıyıcı "seri yok" diye yutardı |

**Testler.** Dikiş: `metrics::tests::export` (iki exporter + kanal
sink'i: sink'in aldığı her rapor — kapanıştaki dahil — iki exporter'dan
sırayla geçti). Prometheus bayt kimliği: `metrics::tests::golden`
değişmedi; tablo mutasyonları (help, tür, sıra) onu kırar.
`/metrics` feature'a göre: `http::tests::the_metrics_path_serves_the_exposition_only_when_compiled_in`.
OTLP eşlemesi: `metrics::tests::otlp::golden` (altın rapor + iki mantık
sayacının okunur dökümü, `golden/otlp.txt`'e sabit),
`metrics::tests::otlp::cross` (yukarıda, §3), `otlp::tests::wire` (alan
numaraları/tel türleri derive'dan bağımsız). Devir:
`otlp::tests::handoff` (vadesi gelmeyen ne devredilir ne sayılır; dolu
yuva düşürür + sayar, sayı sonraki devirle gider; giden görev = düşüş;
vadeler sabit ızgarada — biraz geç gelen rapor sonraki vadeyi
kaydırmaz).
Uçtan uca: `metrics::tests::otlp::push` (test içi HTTP alıcısı protobuf'u
çözer; 503 + cevapsız eş (zaman aşımı) + 200 → üçüncü itme iki hatayı
taşır), `gsb-server` `tests/otlp_export.rs` (feature'sız: tablo
başlatmayı durdurur; `otlp` ile: sunucunun kendi raporları alıcıya
ulaşır, `https`/`0` aralık başlatmayı durdurur; örnek config'in yorumlu
bloğu belgelendiği gibi ayrışır). Mutasyon denetimi: tür başına eşleme
(counter→gauge, oda gauge'u int, monotonic/temporality, ince histogramın
taşma kovası, log2 sınırları, `_total`, MAX→sum, taşma gauge'u, oda
öznitelik anahtarı), düşür-ve-say (sayma yok, derin kuyruk, sayı
taşınmıyor, her rapor vadeli, vade rapordan hesaplanıyor), itme (hata sayılmıyor, 2xx dışı kabul,
zaman aşımı yok, https kabul), üç tel etiketi, sunucu (feature'sız tablo
yok sayılıyor, exporter kurulmuyor, hatalı hedef yutuluyor, bilinmeyen
anahtar kabul) — hepsini bir test yakaladı.

**Sıradaki: `metrics` fasadı exporter'ı** (üçüncü exporter, kendi
feature'ı). Tasarım yeri hazır: aynı aile tablosunu yürüyüp her raporda
`metrics::counter!(…).absolute(v)` / `gauge!(…).set(v)` basan bir
`Exporter` — global recorder yalnız exporter'ın içinde, aktör yolunda
değil (eski nottaki "makrolar global recorder'a yazar" itirazı böylece
aktörlere değmez). Bütçe-göreli histogram kenarları fasadın histogram
modeline oturmaz (örnek başına `record`); o aile fasatta gauge'lara
(`p50`/`p99`) ya da hiç açılmamaya düşer — karar o turun.

**NOT-DONE:** OTLP'de TLS (`https`), gRPC, gzip, yeniden deneme/kuyruk,
DELTA temporality, exemplar'lar, `actions_dropped_top`; exporter başına
toplayıcı periyodundan (1 sn) kısa aralık.
