# Tick & Aksiyon Mimarisi — Tasarım Tartışması (CANLI)

Durum: **karar verildi, uygulandı** (bu commit). Mimari şimdi
`DESIGN.md` §3/§4/§9/§10'da; bu dosya tasarım tartışmasının kaydı olarak
kalıyor (canlı bölüm §5 karar tablosu).

## 1. Hedef mimari (kullanıcının tanımı)

1. **Tick, bir broadcast channel üzerinden akar.** Tek bir ticker görevi
   her periyotta `TickInfo`'yu broadcast channel'a atar. Her room/map
   aktörü bu tick'i bekleyen tek await döngüsündedir; tick gelince işlemeye
   başlar.
2. **Room, tick sırasında kullanıcı kanalından `try_recv` (try_get) ile
   aktif işlemleri çeker** — non-blocking, tick içinde bekleme yok.
3. Çekilenler **component'e map'lenir veya event olarak register edilir**,
   sonra system'ler işler.
4. En son **son durum herkesin out channel'ına** gönderilir, tick biter.
5. **Her user (connection) aktörü, kullanıcıdan veri geldikçe kendi
   action channel'ına yazar** (odanın mailbox'ı değil, user-başına kanal).
6. **Her oda aktörü kendi zaman sapması (drift) hesabını kendisi
   ilerletir** — oda arası koordinasyon yok.
7. **Hiçbir tick'te bekleme (await) yok.**

Not: "Tek loop tüm odaları yürütüyor" ifadesi *sürükleyici* anlamda
değil; tick bir **kanal mesajı** olarak taşınır, odalar bağımsız
görevlerde kendi döngülerinde çalışır (kullanıcı düzeltmesi).

## 2. Mevcut yapıyla uygunluk değerlendirmesi

### Uyan kısımlar (mevcut yapıda birebir var)

| Hedef | Mevcut durum |
|---|---|
| 4 faz: çek → convert → systems → broadcast | `tick_once` senkron: `READ` → `ingest` → `update` → `broadcast` ✓ |
| Tick gövdesinde await yok | `tick_once` tamamen senkron fn, flush `try_send` ✓ (actor'ün tick *arasındaki* `recv().await` idle beklemedir, tick içi değil) |
| User aktörü veri geldikçe yazıyor | `ConnectionActor::forward_to_room` her frame'de yazıyor ✓ (ama hedef: ortak mailbox değil, user-başına kanal — aşağıda) |
| Son durum herkese | `OutSink` → bağlantı-başına `FrameBatch` ✓ |

### Uymayan kısımlar

**A. Saat:** Mevcutta oda-başına pacer görevi (N bağımsız timer). Hedef:
tek ticker + **broadcast channel**.

**B. Aksiyon yolu:** Mevcutta tek ortak oda mailbox'ı (`Tick` + `Action` +
join/leave aynı kuyrukta; room actor mesaj döngüsünde aksiyonları lokal
`pending` Vec'ine biriktirir, tick'te `mem::take`). Hedef: **user-başına
action channel**, room tick sırasında `try_recv` ile çeker.

### Değerlendirme

- **B (aksiyon yolu) — hedef tasarım mevcut olandan İYİ, benimsenmeli.**
  User-başına kanal = user-başına backpressure + izolasyon: tek flood
  yapan kullanıcı yalnızca kendi kanalını doldurur; tick asla aksiyonun
  ardında sıraya girmez, diğer kullanıcılar etkilenmez. Mevcut tasarımın
  bilinen zaafını (ROADMAP P1: "Tick ve Action aynı bounded mailbox'ta")
  kökünden çözer. Maliyet: tick başına O(kullanıcı) `try_recv`
  (boş kanalda ~ns; 100k kullanıcılı odada ~3M poll/sn — idare edilir)
  + kullanıcı başına kanal object'i.
- **A (saat) — broadcast tick tasarımı sağlam; aşağıda detay.**
  Önceki endişem "tek loop odaları tek thread'de sürüklerse 100k'da
  seriye düşer" idi; kanal tabanlı broadcast okumasıyla bu endişe
  geçersiz — odalar bağımsız görevlerde kalıyor, paralellik korunuyor.

## 3. Broadcast tick tasarımı — analiz

### Mekanizma

```text
ticker task (tek, composition root / registry ömrü)
   │  her period: tick_tx.send(TickInfo { tick: u64, at: Instant })
   ▼  (tokio::sync::broadcast, buffer ~64 = 2s @30Hz)
   ┌──────────────┬──────────────┬──────────────
   ▼              ▼              ▼
room actor R1    room actor R2   room actor R3   (bağımsız tokio görevleri)
   │  tek await: tick_rx.recv()
   │  tick gelince (senkron):
   │    1. control kanalını try_recv (Join/Leave/Shutdown)
   │    2. her conn action kanalını try_recv (cap'li)
   │    3. convert → systems → broadcast (out kanalları)
```

Room actor döngüsü (tek await, select yok):

```rust
loop {
    let tick = match self.tick_rx.recv().await {
        Ok(t) => t,
        Err(broadcast::error::RecvError::Lagged(_)) => continue, // catch-up
        Err(broadcast::error::RecvError::Closed) => break,      // ticker durdu
    };
    self.tick_once(&tick); // dt = now - last; Lagged sırasında geçen
                           // süre dt'ye doğal olarak girer
}
```

### Avantajlar

1. **Oda yaşam döngüsü basit:** oda oluştur = `tick_tx.subscribe()`
   (ucuz, protokolsüz); oda imha = control `Shutdown` (≤1 tick) ya da
   ticker kapatma. Mevcut pacer'ın `JoinHandle` + abort bookkeeping'i
   (`RoomEntry.pacer`) kalkar.
2. **Global tick numarası:** tüm odalar aynı `tick: u64` referansında →
   cross-oda tutarlılık, replay, debug, metrik.
3. **Coalescing bedava:** receiver geride kalırsa `Lagged(n)`; büyük
   buffer + dt-doğrulu tick sayesinde oda "1-2 tick geride, gerçek
   hızda" koşar (dt biriken süreyi kapsar). Kalıcı >buffer sapması
   zaten ölüm hali (metrik gösterecek).
4. **Join/leave tick sınırında işlenir** (≤1 tick gecikme): simülasyon
   durumu olaylar için deterministik (MOBA'da spawn/ability timing'i
   tick granüler). Çoğunlukla **özellik** olur.
5. **Ticker bloklanamaz:** `broadcast::Sender::send` non-blocking;
   ticker kodu tek `sleep` + tek `send` döngüsü. Ticker düşerse tüm
   receiver'lar `Closed` alır → global kapanış sinyali.
6. **Drift oda-başına:** her oda `last: Instant` + (isteğe bağlı)
   `tick - expected` eşiğiyle kendi sapma hesabını yapar. Oda arası
   koordinasyon yok — mimari ilke ile birebir.

### Karar gerektiren noktalar (~~AÇIK~~ KAPANDI)

> *Kapandı — hepsi kararlaştırıldı ve uygulandı: 1–5 → §5 karar kaydı
> (#3–#7); 6 → `RoomConfig` varsayılanları `control_capacity: 128`,
> `action_capacity: 256`. Aşağıdaki liste tasarım anının kaydıdır.*

1. **Tek global tick hızı mı?** v1'de tüm odalar aynı hızda en basiti.
   Farklı hız ihtiyacı (60Hz arena + 15Hz lobby) *bölücü* ile çözülür:
   ticker global (en yüksek) hızda atar, oda her k'inci global tick'te
   çalışır (`global_hz % room_hz == 0` config doğrulaması); çalışmadığı
   tick'lerde `last = now` günceller, gerisi boş uyanma (~ns).
   → Öneri: gün-1'de bölücü desteği (5 satır), uniform config varsayılan.
2. **Catch-up dt cap'i mi?** Lagged'dan sonra tek catch-up tick'te dt
   sınırsız olabilir (2s buffer = en kötü 2s dt). Öneri: cap (örn. 4
   periyot) + üstü log/metric; simülasyon geçici olarak geride kalır,
   istemci interpolasyonu kapatır.
3. **Action channel: `try_send` + drop mu, `send().await` (per-conn
   backpressure) mı?** → Öneri: `try_send` + drop sayacı (out tarafıyla
   felsefe uyumu; user kendi kanalını doldursa yalnızca kendi girdisi
   düşer, hafıza bounded).
4. **Join/leave ≤1 tick gecikme** kabul edilebilir mi? (Öneri: evet,
   deterministik tick-sınırı olayı olarak belgele.)
5. **Pacer kodu kalsın mı, temiz geçiş mi?** → Öneri: temiz geçiş
   (dual-mode karmaşıklığı yok); gerekirse git geçmişinden geri alınır.
6. **Control channel kapasitesi** (Join/Leave/Shutdown; düşüktür, 128)
   ve **action channel kapasitesi** (conn-başına; 256 önerisi).

### Olası riskler ve yanıtları

- **Thundering herd:** tüm odalar aynı anda uyanır (tick sınırında CPU
  tepeciği). v1 oda sayısı (onlarca) için ihmal edilebilir; tokio
  worker'lar arası dağıtım zaten var. Jitter yapılamaz (paylaşılan
  faz) — içsel, kabul.
- **Ticker ortak servis:** tek sleep-loop görevi; kodu trivial,
  bloklanamaz, düşerse global kapanış. Kabul edilebilir tek nokta.
- **Boş oda tick'lerine devam eder** (boş tick ucuz: pull boş, sistem
  boş, broadcast boş).
- **`Lagged` sonrası davranış:** yukarıdaki döngüde `Lagged → continue`;
  sonraki `Ok(t)`'de dt tüm biriken süreyi kapsar → stall başına tam
  olarak **tek** catch-up tick.

## 4. Uygulama planı — UYGULANDI ✓

Aşağıdaki adımlar birebir uygulandı (adım numaraları aynı):

1. `gsb-core::ticker`: `TickInfo { tick: u64, at: Instant }` (Clone +
   Send + Sync); `spawn_ticker(period, sender)`; kanal composition
   root'ta `broadcast::channel::<TickInfo>(64)`.
2. `RoomActor` yeniden yapı:
   - Alanlar: `tick_rx: broadcast::Receiver<TickInfo>`,
     `control_rx: mpsc::Receiver<RoomControl>`,
     `conns: HashMap<ConnectionId, RoomConn { out: mpsc::Sender<FrameBatch>,
     actions: mpsc::Receiver<Action>, entity: EntityId }>`;
     `pending` Vec'i kaldırılır (tick'te doğrudan pull, cap'li).
   - `RoomMsg` → `RoomControl { Join { conn, out, reply },
     Leave { conn, entity }, Shutdown }` (sadece mpsc control kanalı).
   - `spawn_pacer` kaldırılır; pacer'ın drift-resync mantığı yerine
     oda-başına `last: Instant` + cap'li catch-up.
   - Join anında oda `actions` çiftini kurar; `a_tx` join reply'sinde
     conn actor'a gider (`(EntityId, Mailbox<Action>)`).
3. `Registry`: `RoomEntry { control: Mailbox<RoomControl> }` (pacer yok);
   `Registry::new(inbox, self_mailbox, factory, ticker: broadcast::Sender<TickInfo>)`;
   CreateRoom → `ticker.subscribe()`; dispatcher Join/Leave → control
   kanalı; Shutdown → tüm odalara control `Shutdown` + break.
4. `RoomConfig`: `tick_hz` = global hızın bölücüsü (v1 default = global);
   `mailbox_capacity` → `control_capacity` + `action_capacity`;
   `max_pending_actions` pull cap'i olarak kalır.
5. `conn.rs`: `InRoom` durumu `Mailbox<Action>` tutar (room mailbox yok);
   forward = `try_send` + drop sayacı.
6. `gsb-server`: ticker oluştur (channel + task) → registry'ye sender;
   `ServerHandle` + ticker `JoinHandle`; `stop()` → registry Shutdown +
   ticker abort.
7. Config: `tick_hz` global tick hızı.
8. Testler:
   - Core: elle tick besleyen deterministik room testleri (mevcut
     pacer zamanlaması kalkar — testler daha deterministik olur).
   - **Lag/catch-up:** ticker'ı 5 tick bırak, room'un tek catch-up tick
     çalıştırdığını + dt toplam süreyi kapsadığını doğrula.
   - **Per-user flood:** bir user kanalını doldursun; diğer user'ların
     aksiyonlarının her tick'te hâlâ işlendiğini + tick'in
     gecikmediğini doğrula.
   - Join tick-sınırı gecikmesi; room imhası (control Shutdown ≤1 tick).
   - Registry testleri (manual ticker ile) güncellenir.
9. Doküman: `DESIGN.md` §3/§4/§9/§10 güncelle; ROADMAP'te
   "tick/action kanal ayrımı" ve "pacer" maddeleri kapatılır; bu dosya
   arşive alınır.

## 5. Karar kaydı

| # | Soru | Öneri | Durum |
|---|---|---|---|
| 1 | Per-user action channel | Kabul | **ONAYLANDI — uygulandı** |
| 2 | Broadcast tick channel (tek ticker) | Kabul | **ONAYLANDI — uygulandı** |
| 3 | v1 global tek tick hızı + bölücü desteği | evet, gün-1'de bölücü | **ONAYLANDI — uygulandı** (`run_every` + `TickRate` doğrulaması) |
| 4 | Catch-up dt cap (4 periyot) | evet | **ONAYLANDI — uygulandı** (`max_catchup`; kare hızından bağımsızlık testiyle doğrulandı) |
| 5 | Action: try_send + drop | evet | **ONAYLANDI — uygulandı** (conn actor `try_send` + drop uyarısı) |
| 6 | Join/leave tick sınırında (≤1 tick gecikme) | evet, özellik | **ONAYLANDI — uygulandı** (CONTROL fazı; determinizm olarak belgelendi) |
| 7 | Pacer: temiz geçiş | evet, kod kaldırılır | **ONAYLANDI — uygulandı** (`spawn_pacer` silindi, dual-mode yok) |

Ek kullanıcı şartı (4. maddeyle birlikte onaylandı): **kare hızından
bağımsızlık** — "frame 15 de olsa, 100 de olsa, gerçek hayata göre yer
değiştirme aynı olsun." `dt` duvar saati tabanlı olduğundan sağlanıyor;
`gsb-game/tests/frame_independence.rs` bunu 60 Hz (run_every=1) vs 15 Hz
(run_every=4) odalarıyla kodluyor: 5.0 s simülasyon süresi her iki odada
aynı mesafe (±0.1 birim).

### Tick saati (BACKLOG F10, 2026-09-26)

**Sorun.** Ticker tick'leri `std::time::Instant` ile hem zamanlıyor
hem damgalıyordu. Tokio'nun duraklatılmış saatinde (`start_paused`)
ticker'ın `sleep`'i sanal saati bir periyot ilerletir ama duvar saati
yerinde durur: damgalar mikrosaniye arayla gelir, odanın `dt`'si (iki
damga arası) donar, yürüyen bir birim yerinde sayar (KIT-ARCHITECTURE
W2-4). Yürüyen aktör testleri gerçek saatte koşmak zorundaydı.

**Karar.** Ticker **runtime saatinde** zamanlar (`sleep_until`) ve
damgalar (`tokio::time::Instant::now()`); `TickInfo::at`'in tipi
değişmedi (`std::time::Instant`, `into_std()` ile) — alanı okuyan ve
`TickInfo` kuran her kod (testlerin elle kurduğu tick'ler dahil) aynen
derlenir. Üretimde değer aynı: tokio'nun `test-util` özelliği yokken
`tokio::time::Instant::now()` doğrudan `std`'ninkidir; varken de hiç
duraklatılmamış bir runtime aynı anı okur. Duraklatılmış saatte damga
sanal saatle ilerler: `dt` periyottur (zamanlayıcının milisaniye
çözünürlüğüyle 33/34 ms, birikimli sapmasız).

**Tüketiciler — kararlar tek tek.** Damgayla KARŞILAŞTIRILAN her okuma
aynı saatten gelmeli (`gsb_core::ticker::now()`); yalnız iş ölçen
okumalar duvar saatinde kalır.

| Tüketici | Saat | Gerekçe |
|---|---|---|
| `dt` (oda ve shard: `t.at − last_at`, catch-up tavanı) | damga | iki damga arası; paused'da periyot, gerçekte değişmedi |
| Tick gecikmesi `late_us` (adım başı − damga) | `ticker::now()` | damgayla fark; std'de paused damga ileride kalır, fark doyarak 0 olurdu — anlamsız bir karışım |
| Girdi-boşta saati: `touch`/süpürme (`t.at`) ve katılımdaki `start` damgaları (oda `join` ×2, shard `session`/`messages`) | damga / `ticker::now()` | süpürme `t.at − start`'ı ölçer: iki uç aynı saatte olmalı |
| Adım süresi (`step_us`, histogramlar, `observe_step_us`) ve alt ölçümler (sınır/göç süreleri) | `std::time::Instant` | CPU işini ölçer; paused saat senkron işte ilerlemez → hep 0 okurdu |
| Ayrılma bekleme süresi (park grace/tavan: `park` ve 0c süpürmesi), RPC zaman aşımı (`due` ve 0b süpürmesi) — oda ve shard | `ticker::now()` (F16) | iki ucu da aynı saatte; paused saatte durur, paused zamanla dolar (aşağıda "F16") |
| Bağlantı aktörünün pencereleri (auth denemeleri, HEARTBEAT_ACK kısması, metrik boşaltma), rUDP demux'ının idle heap'i ve yazıcının RTO/canlılık saati | `std::time::Instant` (değişmedi) | damgayla karşılaşmaz, tick'e bağlı değil; paused saatte sınayan test yok (tetik yok) |
| Bağlantı aktörünün girdi hız sınırı (E1 token bucket: dolum = son varıştan geçen süre × hız) | `ticker::now()` | damgayla karşılaşmaz ama hız "odanın saniyesi başına"dır: paused saatte sanal bir saniye bir saniyelik dolum olmalı — duvar saatinde mikrosaniye olur ve tam sınır hızındaki dürüst istemci reddedilirdi (`gsb-core/tests/input_rate.rs` paused saatte koşar; duvar saatine çevirmek onu kırar). İki ucu da aynı saatte; üretimde aynı an; kapalıyken hiç okunmaz |
| Metrik toplayıcı | yalnız tick olayı, damga okumaz | — |

**F16 (2026-09-26).** Ayrılma bekleme süresi ve RPC zaman aşımı F10'da
std saatte bırakılmıştı; paused saatte gerçek zamanlı akıyorlardı:
paused bir testte park'ın grace'i ya da bekleyen isteğin süresi hiç
dolmuyordu (sanal saniyeler gerçek mikrosaniyelerde geçer). Artık
ikisinin de iki ucu `crate::ticker::now()`'dadır — oda ve shard
aktöründe: `RoomConn::park` (DETACH yolu; shard'da `detach_player`)
mutlak grace/tavan anlarını onunla kurar, 0c süpürmesi onunla okur;
isteğin `due`'su (2c) ve 0b zaman aşımı süpürmesi de öyle. Worker'ın
kaynak koruması zaten `tokio::time::timeout`'tu. Tipler değişmedi
(`std::time::Instant`, `into_std()`), göçte taşınan mutlak anlar
(`ShardMsg`'nin `detach_deadline`/`detach_ceiling`'i) aynı saatte
kalır. Üretimde davranış aynı: `test-util` yokken ya da runtime
duraklatılmamışken `ticker::now()` std'nin anıdır. CPU süresi ölçen
okumalar (adım süreleri, sınır/göç süreleri) std'de kaldı.
Elenenler: (1) *Tick damgasını (`t.at`) kullanmak* — park CONTROL
fazında (`handle_control`) olur, elinde damga yoktur ve zaman aşımı
kaydı damganın değil kaydın anını ister; `ticker::now()` damgayla aynı
saattir ve her yerde okunur. (2) *Alanları `tokio::time::Instant`
yapmak* — `ShardMsg`'nin public alanlarını ve testlerin elle kurduğu
anları değiştirirdi; `into_std()` aynı anı verir (F10'un gerekçesi).
(3) *Testlerde anları elle kaydırmak* (`Rig::age`) — sweep'i sınar ama
canlı aktörü paused saatte koşturmaz; yeni testler ikisini de ister.
Testler: `room::tests::hold::paused` (grace ve tavan),
`shard::tests::hold::paused` (grace), `tests/rpc/paused.rs` ve
`tests/rpc_shard/paused.rs` (5 sn'lik zaman aşımı sanal saatte, gerçek
< 2 sn; kayıttan önce 300 ms gerçek bekleme, karışık saatli bir
karşılaştırma şans eseri geçemesin). Beşi de eski kodda düştü; sekiz
okumanın her biri tek tek std'ye geri alındığında en az biri düşer.

**Elenenler.** (1) *`TickInfo::at`'i `tokio::time::Instant` yapmak:*
tipte açık, ama public alan — `IdleView`, idle saati ve `TickInfo` kuran
her test (~15 dosya) değişirdi; `into_std()` aynı anı verir. (2) *Yalnız
damgayı çevirmek, zamanlamayı std'de bırakmak:* paused'da da sonuç aynı
çıkar (her uyku bir periyot), ama iki saatli ticker'ın geri-kalma
(resync) denetimi karışık saatte kıyas yapardı. (3) *Testlere sanal
`dt` enjekte etmek:* motorun gerçek yolunu değil bir yan yolu sınardı.

**Testler.** `ticker::tests::ticks_are_stamped_on_the_runtime_clock`
(paused: ardışık damgalar periyot ±1 ms, 30 periyot sapmasız);
`room::tests::paused_clock::a_walker_covers_its_distance_on_the_paused_clock`
(canlı ticker'daki oda, 7 m/sn'lik yürüyücü: 60 adımda 2 sn paused
zaman, yol = hız × geçen süre ± bir periyot, gerçek süre < 1 sn). İkisi
de eski damgayla düştü (damga aralığı 13 µs). Mutasyon (yalnız damga
`std::time::Instant::now()`'a geri): ikisi ve çevrilen Cephe testi
düşer. Cephe'nin tek gerçek saatli senaryosu
(`fog::an_enemy_is_seen_through_a_far_tower_on_another_shard`) paused
saate çevrildi: 2,04 sn → ~0,05 sn; görülme/kaybolma tick'i ve mesafe
(38 / 41, 59,82 m) iki saatte de her koşuda aynı.
