# gsb: Reconnect/Detach Tasarımı — Kopan Oyuncunun Politikası

> Durum: TASARIM (uygulanmadı). Bu doküman uygulamanın sözleşmesidir;
> kodla çelişirse kod ya da bu doküman hatalıdır ve ikisinden biri
> düzeltilir. Karar tabloları "Kararlar" bölümündedir; elenen
> alternatifler bölümlerinin içinde saklıdır.

## 1. Amaç ve kapsam

Bir oyuncunun bağlantısı koptuğunda entity'si **kaybolmak zorunda değil** —
ve kaybolup kaybolmaması **oyun kuralıdır**, taşıma katmanının değil. Bu
doküman üç şeyi tanımlar:

1. **Detach politikası** — kopan oyuncunun entity'sine ne olur (§3);
2. **Reattach mekanizması** — geri gelen oyuncu nasıl aynı varlığa takılır
   (§5–§7);
3. **Oda-yok uçları** — resume/connect bir olmayan odaya düşerse ne olur
   (§8).

Kapsam **dışı**: sunucu restartı arasında süren oturum (v1'de tüm park
defteri bellektedir; restart = herkes fresh), dünya durumunun kalıcılığı
(kalıcılık katmanı v1 dışı — bkz. DESIGN §10), cross-region oturum taşıma.

## 2. Bugün neden yetersiz

`ConnIn::RoomGone` / transport ölümü → bağlantı actor'ü teardown → registry
bağlılığı siler → odanın `Leave`'i entity'i gömer. Kopma ile ayrılma aynı
kapıdan geçer; `RoomLogic::on_leave`'in "bu oyuncu dönebilir mi?" diye
sorabileceği bir yer yoktur. Ticket-auth kimliği taşır (`ValidatedTicket.
player`) ama kimliğe hiçbir şey bağlı değildir: düşen socket = kalıcı
despawn (dış inceleme bulgusu — MOBA/MMO için table-stakes boşluk).

## 3. Temel ilke: kopmak bir gerçektir, ne olacağı politika

```
Transport koptu            (gerçek — pump fark eder)
  └─ Detach kararı         (politika — RoomLogic söyler)
        └─ Hold sürer      (mekanizma — oda tick'i süpürür)
              └─ Çözüm     (geri dönüş YA da bitiş)
```

### 3.1 `RoomLogic` yüzeyi (üç ekleme)

```rust
/// Bağlantı koptu: entity'nin kaderini oyun söyler.
fn on_disconnect(&mut self, world: &mut W, conn: ConnectionId,
                 identity: &str) -> Detach;

/// Hold sırasında (her tick'in CONTROL fazında sorulur):
/// detach şimdi sona erdirilebilir mi? (combat-held politikası burada
/// "hayır" der; rakip uzaklaşınca "evet" döner.)
fn may_release(&mut self, world: &mut W, conn: ConnectionId) -> bool {
    true // varsayılan: mantık-vetosu yok
}

/// Hold bitti (grace doldu VE may_release) — entity'nin sonu:
fn on_detach_expired(&mut self, world: &mut W, conn: ConnectionId,
                     to: ExpireTo);
```

```rust
enum Detach {
    /// Eski davranış: hemen despawn (lobbi, sohbet).
    Despawn,
    /// Entity yaşar. `grace = None` → yalnız `may_release` karar verir
    /// (combat-held); `Some(d)` → en geç d sonra bitiş.
    /// Bitişte ne olacağı: insan geri dönerse resume, dönmezse `to`.
    Hold { grace: Option<Duration>, to: ExpireTo },
}

enum ExpireTo {
    Despawn,
    /// Aynı entity'yi bot oynamaya devam eder (§7).
    AiHandover,
}
```

### 3.2 Park edilmiş entity simülasyonu — yeni mekanizma DEĞİL

Sistemler zaten bütün World üzerinde akıyor. Park = "girdisi olmayan
varlık": `ingest`'e aksiyon gelmez, `update` çalışır (fountain regen gibi
pasif etkiler bedava), snapshot'a normal dahildir. Tek fark: READ fazı o
bağlantı için çekim yapmaz ve BROADCAST ona batch göndermez (giden kanal
yarısı ölü).

## 4. Kimlik ve park defteri

Anahtar `ValidatedTicket.player`'dir (ticket-auth zaten döndürüyor; local-
auth yolunda `Auth.name` — bu yolda resume yalnız demo amaçlıdır). Park
defteri **`RoomLogic`'te yaşar**: `identity → park durumu (entity, hold
başlangıcı, meta)` — core generic kalır, oyun dünyasını bilir.

Registry tarafında ise yalnız şu değişir: `ConnClosed`, bağlılık silerken
artık **despawn emri göndermez**; `RoomControl::Detach { conn, entity }`
gönderir ve oda politikaya göre davranır. Bağlılık tablosuna "detached"
işareti düşer: oda hâlâ bilinir, bağlantı yoktur.

**Slot muhasebesi:** park edilen oyuncu oda kapasitesinden yer tutar
(MOBA'da slot onundur). `members` sayacı detach'te düşmez; resume yeniden
aynı slotu kullanır; expire'te düşer.

## 5. Reattach: resume akışı

```
Yeni socket → AUTH { ticket } → doğrulama → ValidatedTicket { player, room }
  → registry: ResumePlayer { conn, room, identity }
      → oda/shard'lara broadcast Resume (§6)
          → park defterinde identity VARSA:
                kanal swap (§7) → reply Resumed { entity }
                istemciye: JOIN_ROOM_RESULT { entity } (AYNI wire id!)
                          + one-shot private FULL (taze üye akışı)
          → YOKSA: sıradan Join gibi devam (saydam fresh-join)
```

Kararlar ve gerekçeleri:

- **Seq/ack sıfırlanır.** İstemci tahmin mutabakatı taze full ile kurulur;
  hwm devamlılığı v1 gereksizdir (dış kararla sabitlendi). Girdi
  numaralandırması yeni oturumda 1'den başlar — mevcut "rejoin sıfırlar"
  sözleşmesi resume'u da kapsar.
- **Wire id korunur:** entity aynı kaldığından diğer istemcilerin
  görünümünde kimlik değişmez; AOI delta akışına da sıçrama yapmaz
  (aynı hücrede zaten içerik vardı).
- **Saydam fresh-join düşüşü:** resume gelip defterde kayıt yoksa hata
  ÜRETİLMEZ — sıradan join işlenir. İstemciyi bekleten bir uç yoktur;
  "resumed=false" bilgiyi AUTH_RESULT/JOIN_ROOM_RESULT üzerinden alır.
  TOCTOU (doğrulama sürerken grace'in dolması) bu şekilde kendiliğinden
  çözülür: hangi dal önce işlenirse o kazanır, ikisi de geçerlidir.
- **Çift oturum:** aynı identity park halindeyken ikinci AUTH gelir →
  eskisinin hold'u iptal, yenisi devralır (tek otorite: en son kazanan).
  Eski socket hâlâ açıksa ERROR 9 ile kapatılır.

## 6. Shard rotasyonu: broadcast-resume

`home_shard` spawn noktasına göre rota seçer; oyuncu arada migrate
olmuş olabilir ve registry "hangi shard'ta"yı bilmez. Yönlendirme:

**Karar:** Resume, odanın TÜM shard mailbox'larına broadcast edilir.
Her shard park defterine bakar; yalnız kaydı bulan swap yapar, diğerleri
no-op döner. Yanlış-pozitif riski YOKTUR: join-epoch şeması (iki-tablo,
kuşak karşılaştırması) zaten "bu bağlantı bu shard'ta mı, bayat mesaj mı"
ayrımını tek integer kıyasıyla yapar — Leave broadcast'inde kullanılan
desenin aynısı. Yeni dizin durumu (conn→shard tablosu) GEREKTİRMEZ;
elenen alternatif budur (registry'de migrasyon raporlarını geri akıtmak —
registry↔shard trafiğini ve durumu büyütür, kazancı sıfır).

Tek-kazanan garantisi: identity başına park kaydı tek shard'tadır (entity
tek shard'tadır — migration protokolünün exactly-once değişmezi). İki
shard'ın aynı resume'u kabul etmesi yapısal olarak imkânsızdır; testle
kilitlecektir.

## 7. Kanal swap: `ShardMsg::Resume` / `RoomControl::Resume`

Aksiyon kanalının alıcı yarısı odaya move edilmiştir (sharded odada
migrasyonlarla tekrar move edilir). Kopan oturumun sender yarısı ölmüştür.
Resume mesajı yeni oturumun uçlarını taşır:

```rust
Resume { conn, epoch, identity, out: Mailbox<FrameBatch>,
         actions_inbox: Inbox<Action> }
```

Kabul eden shard: eski (ölü sender'lı) `out`'u ve boş `actions` receiver'ı
atıp yenilerini takar; entity, grup üyeliği ve dünya durumu AYNEN kalır.
Epoch guard: bayat Resume (ölüm/expire sonrası gecikmiş) tek kıyasla reddedilir.

BROADCAST fazı notu: detach anında o bağlantının `out` kanalı ölmüştür;
hold süresince fan-out ona try_send yapmamalı (drop sayacı kirilenmesin).
RoomConn'a `detached: bool` işareti: broadcast atlar, READ çekmez.

## 8. Oda-yok uçları: resume/connect bir Absent odaya düşerse

İki farklı oda sınıfı vardır ve cevapları FARKLIDIR (dış karar):

| | **Ephemeral** (FPS/MOBA maçı) | **Persistent** (MMO haritası) |
|---|---|---|
| Anlamı | maç; bitince varlığı anlamsız | süreklilik arz eden dünya parçası |
| Connect/resume Absent'e düşerse | **ERROR 12** ("room retired") — asla sıfırdan açılmaz | süreç içinde yeniden oluşturulmuş olur (aşağıda) |
| Panik/kaza | `restart_on_panic` kapalıysa ölür | **restart zorunlu** (aşağıda) |
| `DestroyRoom` | normal operasyon | **emeklilik** (id kapanır; joins ERROR 12) |

Mekanizma (bilinçli olarak minimal):

1. `RoomConfig::persistent: bool` eklenir. True iken supervision
   `restart_on_panic=false` olsa bile yeniden kurar (politika config'ten
   değil sınıftan gelir — "sürekli oda panikle ölükalmaz" bir sınıf
   garantisi, ayarlanabilir tercih değil). Yeniden kurulum BOŞ dünya ile
   olur: **kalıcılık katmanı olmadan MMO haritası fresh açılır** — bu,
   eksik kalıcılık seam'inin bilinen sınırıdır ve belgelenir; dünya
   durumunu korumak bu turun konusu değildir.
2. `DestroyRoom` persistent odaya uygulanırsa id "emekli" sayılır
   (registry küçük bir retired-set tutar — operatör-sınırlı, küçük):
   sonraki join/resume ERROR 12 alır. Operatörün kapatma niyeti,
   kaza-sonrası otomatik yeniden kurulumla EZİLMEZ.
3. **Neden "join'de tembel yeniden kurulum" YOK:** süreç içinde
   persistent odanın Absent'e düşebileceği tek yol operatör destroy'idir —
   ve onun doğru cevabı zaten ERROR 12'dir (kaza yolu supervision'ın
   yeniden kurulumuyla kapalı). Tembel kurulum, operatör niyetiyle
   yarışan bir otomatik-davranış eklerdi; elendi. Süreç DIŞI
   (sunucu restartı) durumunda ise yeniden oluşturma zaten kontrol
   düzleminin/platformun işidir: sunucu persistent odalarını
   başlangıçta pre-create eder (bugünkü `room_count` akışı),
   orchestration katmanı süreçi ayakta tutar.
4. ERROR kodu: **12 = room retired** (ephemeral maç bitti VEYA persistent
   oda emekli edildi). Kod 4'ten ayrıştırılmasının sebebi istemci
   kararının farklı olması: 4 = "geçici/bilinmiyor, tekrar dene",
   12 = "kesin bitti, lobbiye dön".

## 9. Bot devri (`ExpireTo::AiHandover`)

- Bot = **bağlantısız girdi kaynağı**. `ingest` sentezini logic yapar
  (bot sürüşü oyun bilgisidir); core'a bot kavramı girmez.
- Wire id KORUNUR: devir diğer istemcilerin görünümünde kimlik
  değişikliği değildir, sadece davranış değişikliğidir. Snapshot akışı
  kesintisiz sürer.
- Reclaim: resume tam olarak bunu yapar — girdi kaynağını insana
  geri takar (§7 swap). Bot'un biriktirdiği hiçbir şey aktarılmaz
  çünkü bot ayrı bir varlık DEĞİLDİR; buff/pozisyon envanterde
  zaten vardır.
- Demo uygulaması: sahte-bot `ingest`'te hedefe doğru MoveTarget
  sentezler (mevcut movement sistemiyle çalışır) — ucuz ve tüm ucu
  gösterir.

## 10. Gözlemlenebilirlik

`RoomSample`'a: `detached` (anlık park sayısı), `resumes`,
`detach_expired_despawn`, `detach_expired_ai`, `resume_rejected_stale`.
Hepsi mevcut sayaç→kanal→toplayıcı hattından; reject-bucket envanter
disiplinine uygun "doğru yolda artış" testleriyle.

## 11. Kenar uçları tablosu

| Uç | Karar |
|---|---|
| Grace TOCTOU (doğrulama sürerken expire) | Saydam fresh-join (§5) — yarışın iki dalı da geçerli |
| Çift oturum (spam reconnect) | En son kazanan; eski socket ERROR 9 (§5) |
| Oda panigi park defteriyle birlikte ölmesi | Fresh-join düşüşü; v1 kabul, belgeli |
| Sunucu restartı | Kapsam dışı; herkes fresh (§1) |
| Park slotu cap hesabı | Detach'te düşmez, expire'de düşer (§4) |
| Pause-abuse / scout-abuse | grace süresi ve tekrar-cezası oyun config'i; base mekanizma verir |
| Combat-lock sonsuz uzatma (harass-lock) | `Hold.grace = Some(üst sınır)` ile tavan; `None` yalnız güvenilir `may_release` sözü olan politikalar için |
| Ölüyken düşme | Politika detayı — respawn sayacı world'te yaşar, otomatik doğru |
| RPC pending detach anında | Bugünkü leave semantiği: pending düşer, late raporlar sessizce atılır (zaten yapısal) |
| rUDP üstünde resume | Transport-agnostic: resume bağlantı katmanındadır; rUDP deneysel statüsünü değiştirmez |

## 12. Test planı (uygulama turunun kilidi)

1. `disconnect_with_hold_keeps_entity_and_slot` — park: dünya durumu
   korunur, members düşmez, snapshot'a dahil, girdi çekilmez.
2. `resume_binds_new_channels_to_live_entity` — swap sonrası girdiler
   işlenir, wire id aynı, other-client görüşünde sıçrama yok.
3. `broadcast_resume_accepted_by_exactly_one_shard` — sharded; iki shard
   aynı resume'u görür, tek kabul.
4. `stale_resume_rejected_after_expire` — epoch guard.
5. `grace_expiry_falls_back_to_ai_handover` / `_to_despawn` — iki ExpireTo.
6. `may_release_veto_extends_hold_until_cleared` — combat-held.
7. `double_session_supersedes_the_parked_one`.
8. `connect_to_retired_ephemeral_room_is_error_12` /
   `persistent_room_rebuilds_after_panic_even_without_flag`.
9. `ai_handover_bot_ingests_while_detached` — demo bot stub.

## 13. Kararlar (dış danışma ile sabitlendi)

| # | Karar |
|---|---|
| 1 | Bot devri ilk turda VAR (`ExpireTo::AiHandover` + demo stub) |
| 2 | Resume'da seq/ack SIFIRLANIR; istemci taze full ile kurulur |
| 3 | Resume TÜM shard'lara broadcast; epoch guard tek-kabul sağlar |
| 4 | Oda sınıfı `RoomConfig::persistent`; ephemeral Absent = ERROR 12, persistent kaza = zorunlu rebuild, `DestroyRoom` = emeklilik |
