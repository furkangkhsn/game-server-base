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
  | `gsb_room_team_export_drops_total` | Dolu/kapalı registry posta kutusunun reddettiği export'lar (sonraki tick aynı kümeyi taşır) |
  | `gsb_room_team_export_records_total` | Kuyruklanan export'lardaki kayıtlar |
  | `gsb_room_team_over_cap_total` | Çekirdeğin mesaj başı tavanlarının (`TEAM_EXPORT_MAX_*`) kestiği kayıt/takım — çıkışta ve girişte |
  | `gsb_room_team_over_budget_total` | Oyunun takım başı export bütçesinin (kit: `with_team_budget`) kestiği kayıt — export çekirdeğe varmadan önce; oyunun politikası, yük altında beklenebilir (A29) |
  | `gsb_room_team_imports_total` | Uygulanan takım import'ları (hub'ın buraya ulaşan röleleri) |
  | `gsb_room_team_import_records_total` | Uygulanan import'lardaki kayıtlar |
  | `gsb_room_team_expired_total` | TTL'in düşürdüğü kaynak yuvaları (sessizleşmiş kaynak) |
  | `gsb_room_requests_refused_congested_total` | Tıkalı bağlantının (son batch'i düştü) yanıt borcu per-connection cap'e ulaşmışken **işlenmeden ve yanıtlanmadan** reddedilen RPC istekleri — F14'ün fırtına sınırı; F15'ten beri `…_rejected_conn_cap_total`'dan ayrı (orada yalnız yanıtlanan cap retleri). Satırda `req_refused=`; RPC-CONTROL-PLANE §3.1 |
  | `gsb_room_sends_closed_total` | Fan-out'un **zaten kapalı** bir bağlantıya denediği batch'ler (`try_send` → Closed): istemci soketini kapatmış (tipik: LEAVE sonucundan hemen sonra), oda ayrılışı/kopuşu henüz işlememiş — bağlantı sonu başına en çok ~1, istemcinin istediği bir kare kaybolmaz. B32'den beri `gsb_room_dropped_total`'dan ayrı: o artık yalnız DOLU kanalı (yavaş istemci — HELP'inin dediği) sayar. Oran göstergesi yok (bağlantı sonlarıyla sınırlı; oranı ayrılış oranıdır). Satırda `sends_closed=` (`dropped_s=`'den sonra), loadgen telinde GSMI, `RESULT`'ta `sends_closed=` (`dropped=`'den sonra, her satırda); RPC-CONTROL-PLANE §8.2 |
  | `gsb_room_requests_dropped_unread_total` | Oturum bittiğinde (ayrılış, despawn eden kopuş, yeniden katılım, resume, girdi-boşta tavanının `leave_room` altında geride bıraktığı park) action kanalında **odanın henüz okumadığı** RPC istekleri — işlenmez, yanıtlanmaz (CONTROL READ'den önce koşar; ayrılıştan hemen önce gönderilenler). Oda defterini kapatan kova: gönderilen = yanıtlanan + retler + `req_refused` + bu. Satırda `req_unread=`; B36, RPC-CONTROL-PLANE §8.3 |

  Etki, göç ve takım aileleri yalnız shard satırlarında hareket eder
  (tek oda aktörü 0 yazar; takım ailesi yalnız `team_exchange`'i
  uygulayan mantıkta — `ShardedTeamRoom`). Takım sayaçlarının ~1 sn
  penceresi `team_exchange_summary` log satırında da; hub tarafı
  (`relays`, `relay_drops`) `team_hub_summary` satırında kaldı. Crystallization olayları (kit) F9'dan beri
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
  `fights_peak` — MAX), savaş demosu `war_kills`. `/rooms` sayaç
  listelemez (yalnız oda id'leri), değişmedi.
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
  girdileri `actions_dropped_closed` (`actions_dropped`'ın aksine — o dolu
  kanalda istekleri de sayar — ayrık). `gsb-metric scope=net` satırında
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
  *Sınır dışı kalanlar.* Eşzamanlı ops bağlantısına **tavan yok**: her
  kabul edilen bağlantı bir görev; süre sınırıyla canlı görev sayısı
  kabul hızı × (5 sn + yanıt yazma + 300 ms) ile sınırlı, ama sayıyla
  değil. Yanıt **yazmanın** süre sınırı yok: başlığını gönderip yanıtı
  hiç okumayan bir eş, soket tamponlarından büyük bir yanıtta (çok odalı
  `/metrics`) görevini tutar. İkisi de localhost sözleşmesinde (B17)
  kabul edilir; port dışa açılırsa ele alınır (BACKLOG).
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
| 4 | **Tek aile tablosu** (`metrics::export::families`: ad, tür, help, okuyucu, sıra); Prometheus render'ı ve OTLP eşlemesi aynı tabloyu yürür | Yeni sayaç iki yüzeye birden girer; ad/tür kayması yapısal olarak imkânsız. Tablo taşınırken Prometheus metni bayt bayt aynı kaldı (altın test; help/tür/sıra mutasyonları onu kırar). Biçime özgü kalanlar: dağılımların şekli, mantık sayaçlarının (ad kümesi rapor anında belli) biçimi |
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
