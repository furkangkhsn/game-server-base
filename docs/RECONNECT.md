# gsb: Reconnect/Detach Tasarımı — Kopan Oyuncunun Politikası

> Durum: UYGULANDI (ROADMAP P1 `[x]` — Tur A core mekaniği, Tur B demo
> park politikası). Bu doküman uygulamanın sözleşmesi olarak geçerlidir;
> kodla çelişirse kod ya da bu doküman hatalıdır ve ikisinden biri
> düzeltilir. Davranış kilidi: `crates/gsb-core/tests/reconnect.rs`
> (detach/resume, epoch-guard, park süresi dolması, ERROR 12),
> `crates/gsb-core/src/{room,shard}/tests/hold.rs` (süreli bekletmede
> `may_release` vetosu ve veto tavanı, §17) ve
> `crates/gsb-server/tests/loadgen_smoke.rs::loadgen_churn_smoke`
> (§14.5 churn profili). Karar tabloları "Kararlar"
> bölümündedir; elenen alternatifler bölümlerinin içinde saklıdır.

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

/// Hold'un süresi dolduktan sonra her süpürmede sorulur (süreli hold
/// deadline'ından itibaren, süresiz hold ilk süpürmeden itibaren):
/// detach şimdi sona erdirilebilir mi? (savaştaki karakter için
/// "hayır" der; savaş bitince "evet" döner. Veto en fazla
/// `RoomConfig::max_detach_hold` kadar uzatır — §17.)
fn may_release(&mut self, world: &mut W, conn: ConnectionId) -> bool {
    true // varsayılan: mantık-vetosu yok
}

/// Hold bitti (grace doldu VE may_release — ya da veto tavanı aşıldı):
fn on_detach_expired(&mut self, world: &mut W, conn: ConnectionId,
                     to: ExpireTo);
```

```rust
enum Detach {
    /// Eski davranış: hemen despawn (lobbi, sohbet).
    Despawn,
    /// Entity yaşar. `grace = Some(d)` → d sonra `may_release`'e sorulur
    /// (çıkış sayacı + savaşta çıkış yok); `None` → yalnız `may_release`
    /// karar verir (combat-held). İkisinde de veto tavanı §17.
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
auth yolunda `Auth.name` — bu yolda resume yalnız demo amaçlıdır). K4'ten
beri aynı kimlik oyunun **karakter anahtarıdır** da (join yönlendiricisi
ve `GameLogic::on_join_as` onu alır — §6'nın sonu; SECURITY §4b). Park
defteri **`RoomLogic`'te yaşar**: `identity → park durumu (entity, hold
başlangıcı, meta)` — core generic kalır, oyun dünyasını bilir.

Registry tarafında ise yalnız şu değişir: `ConnClosed`, bağlılık silerken
artık **despawn emri göndermez**; `RoomControl::Detach { conn, entity }`
gönderir ve oda politikaya göre davranır. Bağlılık tablosuna "detached"
işareti düşer: oda hâlâ bilinir, bağlantı yoktur.

Bu işaret **spekülatiftir**: politika daha cevap vermeden yazılır, çünkü
registry entity'nin kaderine karar vermez. İşareti ÇÖZEN olay odadan
gelir; registry kendi zamanlayıcısıyla eskitmez (karar da grace de oda
tarafı politikadır — combat-held bir park'ın deadline'ı hiç yoktur).

**Slot muhasebesi:** park edilen oyuncu oda kapasitesinden yer tutar
(MOBA'da slot onundur). `members` sayacı detach'te düşmez; resume yeniden
aynı slotu kullanır; park bittiğinde düşer.

**Spekülatif işaret tam olarak üç olayla bırakılır** (aralarında bir
detach'in bitebileceği her yolu kapsarlar):

1. aynı kimlik için bir resume ya da taze oturum (satır yeniden
   bağlanır);
2. odanın bitmesi (destroy/death/`notify_room_gone`) — her bağlılık gibi;
3. odanın kendi **`RegistryMsg::DetachDespawned`** raporu: detached
   satırın entity'si despawn edildi. Bunun **iki** göndericisi vardır ve
   ikisi de gereklidir:
   - `on_disconnect` **`Detach::Despawn`** dediğinde, yani politika park
     etmeyi hiç kabul etmediğinde (`disconnect_grace_secs = 0`'ın şekli).
     Burada hold da deadline da hiç doğmaz, dolayısıyla süpürme bu satır
     için asla çalışmaz — raporu veren, olguyu gören DETACH kolunun
     kendisidir;
   - başlamış bir hold `ExpireTo::Despawn`'a doğru dolduğunda (§14.4
     süpürmesi).

Canlı bir bağlantının geride bıraktığı park (girdi-boşta tavanı,
varsayılan eylem) aynı şekle getirilir: kendi satırında, bağlantının
**park anahtarı** altında (§16.2) — yukarıdaki üç olay onu da aynen
bırakır.

`ExpireTo::AiHandover` kolu **bilinçli olarak bildirilmez**: o hold,
entity bir bot altında canlı, slotunu gerçekten tutarak ve hâlâ geçerli
bir resume hedefi olarak biter (§9) — satır işini yapmaktadır.

Rapor senkron `try_send`'dir (tick/control gövdeleri await'siz kalır);
DOLU bir registry mailbox'ı raporu düşürmez, bir sonraki tick'e yeniden
kuyruklar — düşürülen rapor sızıntının kendisidir. KAPALI olan düşürür
(sızılacak tablo kalmamıştır).

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

**K4 ile etkileşim (GAME-MODULE "K4 — oyuncu kimliği → ev shard'ı").**
`home_shard` artık `(conn, kimlik)` alır — kimlik bu belgenin resume
anahtarının ta kendisi (§4: `ValidatedTicket.player` ya da eski yolda
`Auth.name`) — ve oyunun join kancası (`GameLogic::on_join_as`) aynı
kimlikle kayıtlı karakteri yerleştirir. Sıra değişmedi: kimlikli bir
join önce bu broadcast-resume'dur; yönlendiriciye yalnız bütün shard'lar
"burada değil" dediğinde (taze join) danışılır. Yani park edilmiş bir
oyuncu, yönlendiricinin onun kaydı için seçeceği shard'da değil, park
edildiği shard'da devam eder; park bittiyse taze join kaydına iner.
Kilit: `gsb-server/tests/mmo_home.rs::a_resume_lands_on_the_parked_character_and_a_logout_returns_to_the_save`
(kaydı shard 1'de, park'ı shard 2'de olan karakter shard 2'de resume
ediyor; çıkıştan sonra shard 1'e taze join).

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
| Sunucu restartı | Kapsam dışı; herkes fresh (§1). `stop()`'ta istemci en-iyi-çaba **ERROR 14** (`SERVER_STOPPING`) alır: park defteri süreçle ölür, bu sunucuda resume yok — geri çekil, sonra ya da başka sunucuya bağlan; yeni süreçte aynı kimlikle join saydam fresh-join'dir. Resume semantiği değişmedi (DESIGN §5.6) |
| Park slotu cap hesabı | Detach'te düşmez, expire'de düşer (§4) |
| Pause-abuse / scout-abuse | grace süresi ve tekrar-cezası oyun config'i; base mekanizma verir |
| Combat-lock sonsuz uzatma (harass-lock) | Çekirdekte mutlak tavan: `RoomConfig::max_detach_hold` (varsayılan 10 dk, DETACH anından ölçülür). Tavanda hâlâ duran veto ezilir, hold `ExpireTo`'suna biter, oda bir kez uyarır; süreli ve süresiz hold'a aynı tavan (§17) |
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
6. `may_release_veto_extends_hold_until_cleared` — combat-held;
   süreli hold'da veto ve tavan: `{room,shard}/tests/hold.rs` (§17).
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

## 14. Açık sorunlar ve çözümleri (dış eleştiri turunda kapatıldı)

İlk taslağın uygulama öncesi kapatılması gereken dört mimari deliği ve
beşinci olarak ölçüm planı. Bu bölüm §5–§8'i taahhüde bağlar.

### 14.1 ConnectionId sürekliliği (en büyük delik)

Oda içi her tablo (`conns`, `roster`, `pending`, `queued`, grup üyelik
listeleri) **ConnectionId anahtarlıdır**; yeni oturum YENİ ConnectionId
alır. Resume'daki kanal-swap yalnız yarısıydı: anahtarın kendisi
değişmektedir ve eski anahtar kalırsa registry'nin yeni-conn bağlılık
eşlemesiyle Leave/Despawn rotası kırılır.

**Karar (v1):** resume kabulünde tek geçişlik **`RebindKey(old → new)`**
adımı — oda, conn-anahtarlı tüm tablolarını tek noktadan yeniden
anahtarlar (bağlantı başına girişler küçüktür; maliyet önemsizdir).
Tek nokta olmasının sebebi projenin yapısal-işaret ilkesidir: re-key'i
dağınık bırakmak "yeni tablo ekleyen unutur" sınıfı hatadır (dirty-cell
dersi).

**Yol haritasına not** *(kapandı — TRAIT-ARCHITECTURE §6 Faz 2,
`1c93477`: oda tabloları `PlayerId` anahtarlı; RebindKey tek bağlama
satırının güncellenmesine küçüldü)*: uzun vadeli doğru şekil, oda içi anahtarı
oturum-bağımsız bir `PlayerId`'ye taşımaktır (re-bind ihtiyacını kökten
kaldırır); v1 için ağır refactor olduğundan ertelendi ve P-listeye
işlendi. Resume mekaniği bu geçişi zorlamaz: RebindKey tek noktada
yaşadığından anahtar tipinin değişimi yerel bir değişikliktir.

### 14.2 Park defterinin migrasyonla taşınması

Defter "logic'te yaşar" ifadesi yetersizdi: logic'in ayrı bir HashMap
alanındaysa shard migrasyonu onu TAŞIMAZ ve oyuncu B shard'ına geçince
park kaydı A'da mahsur kalır (broadcast-resume bile bulamaz — defter
yanlış yerdedir).

**Karar:** park meta'sı (kimlik, hold başlangıcı, ExpireTo) **göçen
oyuncu state'inin parçasıdır** — `ShardLogic`'in oyuncu taşıma yükünün
içinde göçer, tıpkı wire id gibi. Yan-tablo değil; migration protokolü
zaten "full state, exactly-once" sözleşmesiyle gelir ve bu meta'yı da
kapsar. Defter-in-logic ifadesi "sorgulama ve politika logic'tedir"
anlamına gelir, depolama world/player state'inindir.

### 14.3 Wire protokolünde resume'un tetik noktası

İlk taslak "AUTH → ResumePlayer" diye akış çizmişti ama istemcinin JOIN
gönderdiği mevcut akışla ilişkisi tanımsızdı.

**Karar:** **yeni opcode YOK.** Ticket-pinli bağlantının
`JOIN_ROOM_REQ`'u örtük resume denemesidir: oda park defterinde kimliği
bulursa kanalları takar ve `JOIN_ROOM_RESULT { entity }`'yi AYNI wire id
ile döndürür; bulamazsa sıradan fresh join işler (saydam düşüş, §5).
Local-auth yolunda resume demo amaçlıdır (`Auth.name` anahtar).

### 14.4 Deadline durumunun sahipliği

Grace'i logic bildirir, süpürgeyi core sürer — durum nerede?

**Karar:** süre sahibi CORE'dur: `RoomConn.detached: bool` +
`detach_deadline: Option<Instant>` detach anında `Detach.grace`
değerinden yazılır. CONTROL fazındaki mevcut sweep mantığı iki dal:

- `grace = Some(d)`: core kendi deadline'ını izler; deadline gelince
  `may_release` sorar — "evet" alanı `on_detach_expired`'e teslim
  eder, "hayır" hold'u uzatır ve soru sonraki her süpürmede tekrarlanır;
- `grace = None` (combat-held): core HER tick `may_release` sorar
  (detached küçük olduğu için ucuz; park nadiren doludur), "evet"
  alanı bitirir.

Her iki kolda da vetonun üst sınırı core'dadır: `detach_ceiling =
detach anı + RoomConfig::max_detach_hold`; tavanda hâlâ duran veto
ezilir (§17). *Tarihçe:* ilk uygulamada süreli hold'a veto hiç
sorulmuyordu ("grace'in kendisi tavandır"); "20 sn sonra çıkış, ama
savaştayken değil" ifade edilemiyordu (KIT-ARCHITECTURE §10, F4
gözlemi) — §17 bunu kapattı.

### 14.5 Ölçüm planı (önce veri)

Davranış testleri yetmez; iki ölçüm maddesi:

- **Mass-reconnect fırtınası:** sunucu restartı sonrası N yüzlerce
  istemcinin aynı anda AUTH+JOIN/resume denemesi (thundering herd) —
  loadgen'e churn profili eklenir (connect→join→kop→yeniden bağlan
  döngüsü); broadcast-resume'un shard sayısıyla doğrusal maliyeti ve
  registry baskısı ölçülür.
- **ERROR 12 ekleminin notu:** additive proto değişikliğidir (yeni kod,
  alan kayması yok) — ama projenin kendi bulgusu olan versiyonlama/
  reserved disiplini turuna girdi olarak not edilir.

## 15. Uygulama sırası (iki alt tur)

- **Tur A — core mekaniği:** Detach/ExpireTo tipleri, `RoomLogic` üç
  ekleme, `RoomControl/ShardMsg::Resume` + kanal swap + RebindKey,
  epoch-guard, deadline sweep (14.4), `RoomConfig::persistent` +
  emeklilik + ERROR 12, metrik sayaçları; §12'nin core-level testleri
  (1–8).
- **Tur B — demo politika:** MOBA-tipi park (grace + base'e alma),
  bot stub (`ingest` sentezi, reclaim), §12 madde 9 ve uçtan uca senaryo;
  mass-reconnect churn profili (14.5).

## 16. Girdi-boşta tavanı ile etkileşim (AFK turu)

`RoomConfig::max_idle_input_secs` (varsayılan **KAPALI**) bu dokümanın
makinesine yeni bir kader EKLEMEZ; ona yeni bir TETİKLEYİCİ ekler.

**Tetikleyici.** Base her üye için, son *aksiyon taşıyan* karesinin
zamanını tutar (yapısal tanım: bağlantı aktörünün odaya `Action` olarak
ilettiği kare — kayıtlı game-band opcode + base-band RPC zarfı;
HEARTBEAT bağlantı aktöründe yanıtlanır ve odaya hiç ulaşmaz). Mantık bu
sinyali `TickCtx::since_input(player)` ile her tick hook'unun içinden
okur; AFK politikası yazmanın yeri burasıdır.

**Tavan ne yapar.** Süresi dolan üye §3'ün AYNI yoluna verilir:
`GameLogic::on_disconnect` çağrılır ve dönen `Detach` ne diyorsa o olur —
`Despawn`, `Hold { grace, to }`, `ExpireTo::AiHandover` dahil. Yani:

- Base **kendiliğinden despawn etmez**; §3'ün "kopmak bir gerçektir, ne
  olacağı politika" ilkesi tavan için de aynen geçerlidir. Tek fark
  "gerçek"in ne olduğudur: taşımanın ölmesi değil, oynamanın durması.
- MOBA'nın **bot devri AFK'da bedava gelir**: politika zaten
  `ExpireTo::AiHandover` döndürüyorsa, AFK kalan oyuncunun kahramanını
  bot devralır ve oyuncu döndüğünde §7 swap'ı ile geri alır.
- Satırın **resume anahtarı** (`RoomConn.identity`) `on_disconnect`'e
  verilir. Tavan arkasında bir `RoomControl::Detach` mesajı olmadan
  tetiklendiği için satır bu anahtarı join/resume anında saklar; boş bir
  anahtar park defterini anlamsız kılardı (§4: defter kimlikle
  anahtarlıdır).

**Park/bot ile çifte sayım YOK.** Detach kolu (her iki aktörde) satırı
girdi saatinden ÇIKARIR, ve AI devri kolu da öyle yapar. Sonuç:

| Satır | Girdi saatinde mi? | `since_input` | Tavan onu görür mü? |
|---|---|---|---|
| Canlı üye | evet | `Some(d)` | evet |
| Park edilmiş (detached) | **hayır** | `None` | **hayır** |
| Bot beslenen (AI devri) | **hayır** | `None` | **hayır** |
| Resume edilmiş | evet (saat sıfırlanır) | `Some(d)` | evet |

**Bot girdisi girdi SAYILMAZ** ve bu yapısal bir sonuçtur, ayrı bir
kural değil: bot sürüşü §9 gereği mantığın `ingest`'i İÇİNDE
sentezlenir, aksiyon kanalını hiç geçmez — yani yukarıdaki tanıma göre
aksiyon taşıyan bir kare yoktur. Aksi hâlde AI devri sonsuz bir AFK
bypass'ı olurdu.

**Shard.** Aynı makine shard aktöründe de vardır. Girdi damgası göçen
oyuncu yüküyle (`PlayerMigration.last_input`) TAŞINIR — §14.2'nin park
meta'sı ile aynı gerekçe: saat per-aktördür, taşınmazsa hareket eden
boşta bir varlık her sınır geçişinde affedilir.

**Varsayılan eylem: üyelik biter, soket kalır — sözleşme (B40).** Tavan
(varsayılan `afk_action = leave_room`) oda ÜYELİĞİNİ sonlandırır, SOKETİ
kapatmaz. Taşıma bağlantı/registry katmanının malıdır ve onun kendi
tavanı (`idle_timeout_secs`) zaten vardır — heartbeat atan bir istemci
tanımı gereği taşıma-boşta DEĞİLDİR. Idle-kick'ten sonra bağlantı,
kendi `LEAVE_ROOM_REQ`'ini göndermiş bir bağlantıyla AYNI durumdadır —
politika ne karar vermiş olursa olsun:

1. **Kimliği doğrulanmış, hiçbir odada değil.** Bağlantı aktörü
   kendini odadan ayırır (registry'nin `ConnIn::LeftRoom`'u ile).
2. **Registry satırı tam da bunu söyler:** satırın odası/varlığı yok.
   Despawn edilen üyenin slotu (ızgarada `ShardGroup` üye slotu) HEMEN
   geri gelir ve `leaves` sayılır; bağlantı sonra kapanınca satır
   sızmadan gider. Park edilen varlık slotunu tutmaya devam eder — ama
   bağlantının satırında değil, **kendi satırında** (§16.2).
3. **Oyun kareleri dışarıdaki her kare gibi yanıtlanır:** `ERROR 6`
   (`NotInRoom`, *race* sınıfı — ayrılma/yeniden katılma penceresinin
   mevcut kuralı; sert ihlal DEĞİL, bağlantı açık kalır). İstemci hiçbir
   şeyi yanlış yapmadı: kod 6 ona "odada değilsin" der, cevabı katılmaktır.
   Bütçe kuralı da aynıdır: kod 6'ya tepki vermeden oyun karesi yollamayı
   sürdüren istemci (race ağırlığı 1, bütçe 16) dışarıdaki-kare
   durumlarının hepsindeki gibi sonunda kapanır.
4. **`JOIN_ROOM_REQ` doğrudan geçer** — önce `LEAVE_ROOM_REQ` gerekmez.
   Park edilmiş bir varlık için bu join §14.3'ün örtük resume'udur: aynı
   kimlik aynı varlığı geri alır; dolu bir ızgarada da (parkın kendi
   slotu sayılır, iki kez değil).
5. **Tel: kick anında hiçbir şey gitmez** (bayt bayt eskisi gibi). Protokolde
   "sunucu seni odadan çıkardı, bağlantı açık" diyen bir kare YOK:
   `LEAVE_ROOM_RESULT` bir isteğin yanıtıdır, `ERROR 5` (oda yıkıldı)
   kapanışla biter. İstemci durumu ilk oyun karesinin `ERROR 6`'sından
   öğrenir. Ayrı bir bildirim yeni bir protokol öğesi olurdu (BACKLOG).

*Düzeltme notu (E6 turunda testle görüldü, B40'ta düzeltildi):* bu
paragrafın önceki hâli "bir sonraki oyun karesi kapalı aksiyon kanalına
çarpar ve bağlantı aktörü kendini odadan ayırır" diyordu; bu yalnız
despawn'da doğruydu. Politika **park** ettiyse satır aksiyon kanalını
tutuyordu: kareler park edilmiş satırın kanalında birikiyor, bağlantı
kendini `InRoom` sanıyor ve doğrudan `JOIN_ROOM_REQ` `ERROR 3` (sert
ihlal) alıyordu. **Despawn** edilen üyenin registry satırı ise canlı
aidiyet olarak kalıyor, bağlantı sonra kapanınca `detached` işaretlenip
slotuyla bir daha bırakılmıyordu. İkisi de `tests/room_close/leave.rs`
ile sabitlendi.

### 16.1 Tavanın eylemi: `afk_action` ve oda→registry kapatma fiili (E6)

AFK'yı odadan mı atmalı yoksa sunucudan mı — bu dağıtımın/oyunun
kararıdır (bakımcı kararı, BACKLOG E6, 2026-09-27). Motor ikisini de
yapı taşı olarak verir, varsayılanı bugünküdür:

| `RoomConfig::afk_action` | Üyelik | Soket | Tel | Sayaç |
|---|---|---|---|---|
| `LeaveRoom` (**varsayılan**) | `on_disconnect` karar verir (park / AI devri / despawn); bağlantı odada değildir (§16, §16.2) | AÇIK kalır | kick anında hiçbir şey; sonraki oyun karesine `ERROR 6` | — |
| `Disconnect` | aynı yol, aynı karar | KAPANIR | en-iyi-çaba `ERROR 9` (`input idle: no game input for N s (…)`) → kapanış | `server_closes{reason="idle_input"}` |

Her iki eylemde de önce §16'nın yolu çalışır — oyunun `on_disconnect`'i
varlığın kaderini seçer; `Disconnect` yalnız TAŞIMAYI ekler. Park/bot
satırları girdi saatinde olmadığından (yukarıdaki tablo) hiçbir eylem
onlara ulaşmaz.

**Fiil: oda → registry → bağlantı.** Oda (ya da shard) politika
koştuktan SONRA `RegistryMsg::CloseConn(CloseRequest { conn, room,
entity, parked, cause, reason })` ister. Tick gövdesi await etmez:
istek odanın kuyruğuna girer ve 0d fazının sonunda `try_send` ile
yollanır (`registry::flush_close_requests`). Registry tek tablo
aramasıyla satırını yerleştirir ve kararı bağlantıya diğer registry
kapanışlarının (`superseded`, doğum cap'leri) yolundan iletir: spawn
edilmiş bir `ConnIn::ServerClosed { cause: IdleInput, reason }` —
registry ne odayı ne bağlantıyı bekler (S kuralı). Bağlantı aktörü
kararı kaydeder, bildirimi `try_notice` ile (senkron `try_send`,
B12'nin kuralı) kuyruğa bırakır ve çıkar; writer pump kuyruğu boşaltıp
soketi kapatır — bildirim kapanıştan önce iner.

**Dolu posta kutusu kuralı: sonraki tick'te yeniden dene, düşürme.**
Registry posta kutusu DOLUYSA istek odanın kuyruğunda (sırasıyla) kalır
ve sonraki tick'te yeniden denenir; KAPALIYSA (registry gitti — süreç
iniyor) düşer. `DetachDespawned` raporlarının kuralının aynısı.
Gerekçe: düşen istek, dağıtımın kapatılmasını istediği soketi açık
bırakırdı — opt-in'in tüm amacı; sayılan bir düşüş hiçbir şeyi geri
getirmez. Kuyruk üyelikle sınırlıdır: bir üyelik bir kez biter, bir
kez ister; doymuş registry yetiştiği an boşalır.

**Registry satırı neden BURADA yerleşir.** Oda üyeliği kendi başına
bitirdi — önünde `RoomControl::Detach` yok — yani satır hâlâ CANLI bir
aidiyet. Bırakılsaydı bağlantının kendi kapanışı onu `detached`
işaretler, odanın yok saydığı bir DETACH yollardı (binding gitti ya da
satır zaten park); despawn edilmiş bir üyenin satırı `max_connections`
slotuyla (ızgarada `ShardGroup` üye slotuyla) sonsuza dek kalırdı.
Kural (`registry/actor/close.rs`):

- `parked` → satır `detached` işaretlenir (taşıma ölümünün satırı gibi;
  park bitişi `DetachDespawned` ya da resume serbest bırakır). İşaret
  HEMEN konur: bağlantının kapanışı işlenmeden park biterse de satır
  bırakılır.
- `parked` değil → aidiyet gider (sharded üye sayısı düşer, `leaves`
  sayılır); taşıması ZATEN ölmüş satır (önce `ConnClosed` geldiyse)
  doğrudan silinir — onu başka hiçbir şey bırakmazdı.
- Bayat istek (`room` VE `entity` tutmuyor: ayrıldı, başka yere ya da
  yeni varlıkla katıldı, satır bırakıldı) sessiz no-op.

Böylece istek, bağlantının `ConnClosed`'u ve odanın `DetachDespawned`'ı
hangi sırada gelirse gelsin aynı son duruma varılır.

**Park + `Disconnect`: satır kalır, kuyruk bırakılır.** Park edilen
satır bağlantının giden kuyruğunun bir kopyasını tutar; writer pump
soketi ancak bütün göndericiler düşünce kapatır. Bu yüzden tavan
`Disconnect` altında park ettiği satırın giden yarısını bırakır
(`RoomConn::release_outbound` — kapalı bir yer tutucu). Park edilmiş
satır zaten bir şey göndermez (BROADCAST atlar) ve resume yeni
oturumun kuyruğunu bağlar. Taşıma ölümünde gerek yok: writer zaten
gitti. `LeaveRoom` da bu satırın iki yarısını bırakır ve satırı park
anahtarına taşır (§16.2).

**Bildirim kodu: mevcut `ERROR 9`, yeni kod DEĞİL.** Kod 9 "sunucunun
bu oturum hakkındaki hükmü" sınıfıdır (idle, cap'ler, bütçeler,
`superseded`, reddedilen akış — DESIGN §5.6); istemci kararı o sınıfın
içinde kalır (`message` hangi hükmün düştüğünü söyler). Yeni bir kod
toplamalı olurdu (base protokol evrim kuralı izin verir) ama istemciye
yeni bir KARAR taşımazdı: kapanıştan sonra tekrar bağlanmak — park
varsa resume ile — kod 9'un zaten anlattığı davranıştır. Bildirim
yalnız opt-in yolda gider; varsayılanda tel bayt bayt aynıdır (testle
sabit). Diğer kod-9 kapanışlarının beklemeli `send_frame`'i yerine
en-iyi-çaba `try_send`: AFK üye okumayı da bırakmış olması en muhtemel
üyedir (arka plana alınmış istemci); beklemeli gönderim aktörü
write-stall penceresine kadar (pencere kapalıysa sonsuza dek) park
ettirir, dağıtımın kapatılmasını istediği soketi açık tutardı.

**Ayar ve oyun dikişi.** `afk_action = "leave_room" | "disconnect"`
düz ve `[rooms.<id>]` içinde (B18 katmanlaması); yazılmazsa oyunun
varsayılanı `GameModule::afk_action()` (sağlanan metot, varsayılan
`LeaveRoom`) — E1'in `GameModule::input_rate` deseni. Tavanın kendisi
(`max_idle_input_secs`) operatörün anahtarı kalır; tavan yoksa eylemin
etkisi yoktur.

**Oyun mantığına açılmadı (bilinçli).** Fiil bu turda yalnız tavanın
eylemi. Genel bir "oyuncuyu at" (ör. `TickCtx`/bir kanca üzerinden)
`GameLogic` + `ShardLogic` + kitin `Game`'ine yeni yüzey ve yeni
anlambilim kararları getirirdi (üyelik hangi yoldan biter — `on_leave`
mi `on_disconnect` mi, gerekçe metni kimin, hız sınırı) — BACKLOG'a
ayrı madde olarak yazıldı. İç fiil (`CloseRequest`, sebep + metin
taşıyan) buna hazır: yeni bir istek kaynağı yeni bir `ServerClose`
etiketi ve bir üretici ekler.

**Elenen alternatifler.** (1) *Registry'nin bağlantı aktörüne kapanışı
yollayıp tabloyu bağlantının kapanışına bırakması* — despawn edilmiş
üyenin satırı sızardı (yukarıda). (2) *Dolu posta kutusunda sayılan
düşüş* — soket açık kalırdı. (3) *Odanın bağlantıya doğrudan yazması*
(elinde yalnız giden çerçeve kuyruğu var) — hüküm sayılmaz, satır
yerleşmez. (4) *Yeni `ErrorCode`* — yukarıda. (5) *`idle_timeout`
etiketini paylaşmak* — taşıma-boşta ile girdi-boşta ayrı sorulardır;
ayrı `idle_input` etiketi.

### 16.2 Varsayılan eylemin mekaniği: bırakılan park ve `LeaveConn` (B40)

§16'nın sözleşmesi üç aktörde uygulanır; E6'nın `CloseConn` yerleşimi
paylaşılır, soket kapatılmaz.

**Oda (ya da shard), tavan anında.** Politika koştuktan sonra
`afk_action = LeaveRoom` ve registry varsa:

- *Despawn:* satır zaten gitti (aksiyon kanalı da onunla kapandı).
  Despawn kolunun taşıma-ölümü raporu (`DetachDespawned`) bu yolda
  **gönderilmez** — satırı istek yerleştirir; ikinci bir yerleşim, çok
  seyrek bir sırada, bağlantının SONRAKİ üyeliğini bırakabilirdi.
- *Park:* satır kalır; bağlantıyla paylaştığı iki yarı bırakılır
  (`release_outbound` — yoksa bağlantı kapandıktan sonra writer soketi
  açık tutar; `release_actions` — bağlantının kareleri okunmayan bir
  kanala düşmesin, bağlantı üyeliğin bittiğini görebilsin) ve park
  **park anahtarına** taşınır: `ConnectionId::park_key()` (üst bit
  ayrılmış; kabul döngüsü 1'den yoğun sayar, oraya varmaz). Binding
  satırı `conn → key` taşınır (shard'da `conn_epoch` da, resume'daki
  gibi), satırın `conn` geri-referansı `key` olur. Bundan sonra park,
  taşıma ölümünün bıraktığı şeyin ta kendisidir — oturumu gitmiş bir
  park — yalnız kimliği bu anahtardır. Canlı bağlantının sonraki hiçbir
  hareketi (bayat bir `Leave`/`Detach`, taze bir join'in "kendi eski
  durumunu ez" adımı, kendi kapanışı) parka dokunamaz. Anahtar
  deterministiktir (paylaşılan sayaç yok); odada aynı anahtarı tutan
  daha eski bir park varsa (aynı oturumun ikinci parkı) taşıma yapılmaz
  ve istek parkı bildirmez.
- İstek kuyruğa girer: `LeaveRequest { conn, room, entity, park }` —
  `park = Some(key)` ya da `None`. 0d fazının sonunda `try_send`
  (`flush_leave_requests`), E6'nın kuralları: DOLU → sonraki tick, KAPALI
  → düşer. Üç ek kural:
  1. **Bekleyen bir `DetachDespawned` raporunun önüne geçmez** (rapor
     kuyruğu boş değilse istekler bekler) — aynı anahtarın bir önceki
     parkının raporu, yeni parkın isteğinden önce varmalı.
  2. **Gönderilirken park yeniden sınanır:** istek beklerken park bittiyse
     (hold doldu — raporu önden gitti — ya da başka bir oturum resume
     etti), taşınacak park kalmamıştır: istek `park = None` ile, despawn
     olarak yerleşir.
  3. **Aynı bağlantı burada yeniden katılır ya da kendi parkını resume
     ederse** bekleyen isteği düşer: yerleştireceği üyelik o bağlantının
     canlı üyeliğidir artık.

**Registry: `RegistryMsg::LeaveConn`.** Tek tablo araması, E6'nın bayat
koruması (`room` VE `entity` satırın şimdiki üyeliği değilse no-op):

- `park = None` → E6'nın despawn kolu (ortak `settle_ended`): aidiyet
  gider, sharded üye sayısı düşer, `leaves` sayılır; taşıması zaten
  ölmüş satır silinir.
- `park = Some(key)` → üyelik **yeni bir satıra** taşınır: `key`
  altında, `detached`, oda + varlık + kimlik kopyalı, inbox yok. Sayaç
  değişmez (park, üyeliğin slotunu taşır). Bağlantının satırında aidiyet
  kalmaz; taşıması zaten ölmüşse satır silinir (tek satır kalır: parkın).
  Park satırını §4'ün üç olayı bırakır, hiçbir özel kod olmadan:
  `DetachDespawned { conn: key }` (oda, parkın satırındaki `conn`'u
  bildirir), aynı kimliğin resume'unun yeniden-bağlama temizliği, odanın
  bitmesi. `key` zaten bir satırdaysa (aynı oturumun başka odadaki
  parkı) istek despawn gibi yerleşir.
- Sonra bağlantıya `ConnIn::LeftRoom { room }` (spawn'lu gönderim,
  beklenmez; taşıması ölmüşse gönderilmez).

**Bağlantı aktörü: `ConnIn::LeftRoom`.** Yalnız `InRoom { room }` ise VE
o üyeliğin aksiyon kanalı kapalıysa (oda bıraktı) `detach()` — tel
sessiz. Arada ayrılıp yeniden katılmışsa yeni üyeliğin kanalı AÇIKTIR:
bildirim bayattır, yok sayılır. Bildirim registry yerleştirdikten SONRA
gönderildiği için bağlantının bir sonraki `SpawnPlayer`'ı registry'ye
yerleşimden sonra varır. Bildirimden ÖNCE kapalı kanala çarpan bir kare
ise eskisi gibi sessizce ayırır; o zaman bağlantının join'i yerleşimden
önce varabilir. Aynı odaya ise 3. kural kapatır; başka odaya ise
registry'nin `SpawnDone` kuralı: hâlâ başka bir odaya bağlı bir satırın
başka bir odaya katılması bildirilmemiş bir bitiş demektir (bir ayrılma
her zaman bir sonraki join'den önce yerleşir — tek dispatcher, sırayla),
eski üyeliğin ızgara slotu orada geri verilir ve geç gelen istek satırı
taşınmış bulup hiçbir şey yapmaz.

**Izgara kapısı.** Sharded odanın cap'i registry'dedir; parkın satırı
sayılır. Aynı kimlikle `detached` bir satırın o odada tuttuğu park
varken gelen join *yeniden katılma* sayılır (cap'e takılmaz) — resume o
sayıyı devralır, `SpawnDone` temizliği netler. Bu, taşıma ölümünden
sonra dolu ızgaraya dönen oyuncunun da `RoomFull` almasını düzeltir.

**Kabul edilen sınırlı kesinsizlik.** İstemci tam kick anında
`LEAVE_ROOM_REQ` gönderir ve registry onu yerleşimden önce işlerse
(registry kuyruğu dolu, istek yeniden deneniyor), `LeaveDone` üyeliği
kapatır ve istek bayat kalır: park, bitene ya da resume edilene dek
registry'de sayılmaz (eksik sayım, sızıntı değil; kendini onarır).
Aynı oturumun iki eşzamanlı parkı (anahtar dolu) ve yukarıdaki
başka-odaya-join yarışında park edilmiş bir üyelik de böyledir. İki
kural (rapor önünde bekleme, gönderimde park sınaması) yalnız gerçek
eşzamanlılıkta fark eder: tek iş parçalı bir testte registry iki
`try_send` arasında boşalmaz — biri (bekleme) bu yüzden deterministik bir
testle mutasyona karşı sabitlenemedi.

**Elenen alternatifler.** (1) *Parkı bağlantının kendi satırında
tutmak* (`detached` işaretli canlı satır) — satır bir üyelik ve bir canlı
bağlantı olamaz: bağlantı başka odaya katılınca parkın slotu kaybolur,
`detached` satırlar süpersedence taramasında atlanır, `DetachDespawned`
yaşayan bağlantının satırını silerdi. (2) *Satıra ikinci bir aidiyet
alanı* — §4'ün her yolunu (resume temizliği, oda sonu, rapor, üye
sayımı) ikinci alan için yeniden yazmak gerekirdi; park anahtarı bu
yolların hiçbirine dokunmaz. (3) *Park anahtarını paylaşılan bir sayaçla
basmak* — küresel durum; deterministik anahtar yeter. (4) *Bağlantının
kendini kapalı kanaldan tembelce ayırması (bildirim yok)* — ayrılan
bağlantının join'i registry yerleşiminden ÖNCE varabilir ve resume
edilen üyeliği yanlışlıkla kapatırdı; parkta kanal zaten hiç kapanmıyordu.
(5) *Kick anında istemciye kare göndermek* — mevcut protokolde uygun kare
yok (yukarıda, madde 5).

## 17. Süreli bekletmede veto ve veto tavanı (`max_detach_hold`)

Amaç: "kopan karakter 20 sn sonra çıkış yapsın — ama savaştayken değil".
İlk uygulamada `may_release` yalnız süresiz hold'da (`grace = None`)
soruluyordu; süreli hold deadline'ında koşulsuz bitiyordu ("grace'in
kendisi tavandır"). Çıkış sayacı ile savaş vetosu birlikte ifade
edilemiyordu (KIT-ARCHITECTURE §10, F4 gözlemi).

**Anlam.**

1. Süreli hold deadline'ına varınca core `may_release`'i sorar. `true`
   → hold bugünkü gibi `ExpireTo`'suna biter (aynı süpürme, aynı
   sayaçlar). `false` → hold uzar.
2. **Yeniden sorma ritmi: her süpürme** (her tick'in 0c fazı). Süresiz
   hold'un zaten kullandığı ritim — tek kural, tek kod yolu
   (`RoomConn::hold_asks`/`hold_end`, iki aktör de aynı yardımcıyı
   çağırır). Veto kalktığı anın ilk tick'inde hold biter: savaş bitince
   karakter gecikmeden çıkar. Maliyet sınırlı: tick başına bekletilen
   satır başına en çok bir çağrı, yalnız süresi dolmuş satırlar; küme
   küçük ve oda kapasitesiyle sınırlıdır (park edilen satır slot tutar,
   §4); `may_release`'in bir bileşen okuması kadar ucuz olması
   sözleşmedir. *Elenen:* sınırlı geri çekilme (1, 2, 4 … sn) — vetonun
   kalkışını gecikmeli görür (savaş biter, karakter saniyelerce daha
   dünyada kalır), satır başına ek durum ister (sonraki soru anı; göçte
   de taşınması gerekirdi); kazancı zaten seyrek olan bir çağrıyı
   seyreltmekten ibaret.
3. **Tavan:** `RoomConfig::max_detach_hold: Option<Duration>`, DETACH
   anından ölçülür (`RoomConn.detach_ceiling`; göçte
   `PlayerMigration.detach_ceiling` ile taşınır — sınır geçişi tavanı
   sıfırlamaz, yeni sahip shard uygular). Tavanda veto hâlâ duruyorsa
   hold `ExpireTo`'suna zorla biter ve oda (shard'da shard aktörü)
   **bir kez** uyarır; sayaç `detach_ceiling_warns`
   (`idle_ceiling_warns` deseni: 0 ya da 1, tracing'e girmeden
   kilitlenebilir).
4. **Tavan yalnız vetoyu ezer, grace'i kısaltmaz.** Grace'i tavandan
   uzun bir hold grace'in sonuna kadar yaşar (o anda veto varsa hemen
   ezilir — tavan geçmiştir). Sıra: önce soru, sonra tavan; tavandan
   sonra gelen `true` olağan bitiştir (uyarı yok), "zorla" yalnız
   gerçekten ezilen bir vetodur. Böylece hiç veto etmeyen oyunda
   davranış bire bir aynıdır (kilit:
   `a_logic_that_never_vetoes_ends_every_hold_where_it_did` ve shard
   eşi).
5. **Varsayılan `Some(10 dk)`** (`DEFAULT_MAX_DETACH_HOLD`): MMO
   demosunun 20 sn çıkış sayacının 30×, kit'in 30 sn varsayılan
   grace'inin 20×, 5 dakikalık bir MOBA terk penceresinin 2×. "Savaşta"
   durumu son vuruştan sonra saniyeler sürer; sahibi hiçbir şey
   yapamazken on dakika sonra hâlâ duran bir savaş, başkasının onu
   canlı tutmasıdır — sınırlanan kilit budur. Maliyet: park edilen satır
   kapasiteden ve registry'den en çok 10 dk yer tutar.
6. **`None` = tavan yok** (veto durdukça tutar — yalnız güvenilir
   `may_release` için; süresiz hold'un eski davranışı). **`Some(ZERO)`
   = uzatma yok:** veto ilk sorulduğunda ezilir — süreli hold
   deadline'ında biter (vetonun sorulmadığı eski davranış), süresiz hold
   ilk süpürmede. Bilinçli olarak literaldir ve
   `max_idle_input_secs`'in "0 = kapalı" kuralından ayrılır: orada
   sıfırın anlamlı okuması yoktu (her üyeyi ilk süpürmede atmak), burada
   güvenli ve kesin bir anlamı var; "0 = kapalı" bir yazım hatasını
   sınırsız kilide çevirirdi. Temsil edilemeyecek kadar uzak tavan
   (`Duration::MAX`) tavansız sayılır (aktör taşmada paniklemez).

**Süresiz hold kararı: tavan onlara da uygulanır.** Harass-lock
vetonun özelliğidir, grace'in değil: süresiz hold SAF vetodur, yani en
açık olan yol odur. Eski "`None` yalnız güvenilir `may_release` sözü
olan politikalar için" kuralı belgede duran, zorlanmayan bir sözdü;
takılı kalan tek bir veto slotu ve registry satırını süreç boyunca
sızdırırdı. Davranış değişikliği dardır — yalnız vetosu 10 dk'dan uzun
duran süresiz hold farklılaşır — ve tam eski davranışı isteyen oda
`max_detach_hold: None` der. *Elenen:* tavanı yalnız süreli hold'a
uygulamak (iki kural; en açık yol korumasız kalır).

**Demo.** MMO'nun politikası "çıkış sayacı + savaşta çıkış yok"tur:
`LOGOUT_GRACE` (20 sn) sonra `ExpireTo::Despawn`, ama isabet eden her
saldırı saldırgana `InCombat { until: tick + COMBAT_TICKS }` (6 sn)
yazar ve `MmoGame::may_release` bu işaret dururken "hayır" der; savaş
sistemi süresi dolan işareti kaldırır, çıkış bir sonraki tick'in
süpürmesinde tamamlanır. İşaret sınır geçişinde `MmoMig` ile göçer.
Gerçek shard aktörleri üzerinden kilit:
`gsb-demo-mmo/tests/combat_logout.rs` (savaştaki karakter grace'ten
sonra savaş bitene dek bekler ve tam o tick'ten sonra çıkar; tavanı
aşan savaş çıkışı zorlar).

**Sunucu config'i: `max_detach_hold_secs`** (küçük paket). Tavan
artık operatörün elinde: `gsb-server`'ın düz anahtarı, barındırılan
HER oyunun odalarına gider (başlangıçta ön-kurulan odalar; sharded bir
oyunda registry her shard'a aynı `RoomConfig`'i verir — tek eşleme
`Config::room_config`; admin yüzeyinin runtime'da açtığı odalar da —
F8). Yazım: saniye (`>= 0`, kesir olabilir) ya da
`"off"` (= `None`, tavan yok); yazılmazsa çekirdeğin varsayılanı
(10 dk). `0` literaldir (= `Some(ZERO)`, uzatma yok) — madde 6'nın
gerekçesiyle. Negatif sayı, başka bir kelime ya da yanlış tip
başlatmada hata (anahtarı ve alabileceğini adlandırır). *Elenen
yazımlar:* "0 = kapalı" (`max_idle_input_secs` geleneği — burada bir
yazım hatasını sınırsız kilide çevirirdi); negatif = kapalı (aynı
risk); ayrı bir boolean anahtar (tek ayar için iki anahtar, çelişebilir).
TOML'da null olmadığı için "yok" bir kelimeyle yazılır. Kilit: parse ve
odaya ulaşma `config/axes/listeners/room/tests.rs`; uçtan uca
`tests/mmo_logout.rs::the_server_ceiling_bounds_the_combat_hold`
(1 sn tavanla savaştaki karakter 6 sn'lik savaş penceresini beklemeden,
1 sn'lik çıkış sayacında çıkar). *Kapandı (F8):* admin HTTP'nin
`POST /rooms/open`'ı odayı eskiden `RoomConfig::default()` + `tick_hz`
ile kuruyordu — bu anahtarı (ve `max_players`, `max_idle_input_secs`
gibi diğer oda anahtarlarını) almıyordu. Artık runtime oda da aynı
şablondan (`Config::room_template`) kurulur; istek başına tek
geçersiz kılma `tick_hz` (OPS §2). Kilit:
`tests/mmo_rooms.rs::a_runtime_room_gets_the_server_ceiling` (aynı
senaryo `/rooms/open` ile açılan odada: düzeltmeden önce savaş
penceresi boyunca, 6,03 sn tutuldu).

**§16 ile ilişki.** İki tavan bağımsızdır: girdi-boşta tavanı üyeyi
`on_disconnect`'e verir; politikanın başlattığı hold her hold gibi
`max_detach_hold`'a tabidir (saati o detach anında başlar).

**Elenen diğer alternatifler.**

- *Tavan = grace'in katı* (ör. 3× grace): süresiz hold'da tanımsız,
  oyuna göre değişen, config'te görünmeyen bir sınır.
- *Grace'i de kesen mutlak üst sınır:* veto etmeyen oyunun davranışını
  değiştirirdi (uzun grace'li bir bekletme 10 dk'da kesilirdi).
- *Oyun başına tavan* (`Detach::Hold`'a alan): `Detach` API'sine kırıcı
  ekleme; tavan operatörün güvenlik vanasıdır (oda config'i), oyun
  kuralı değil — oyun kuralı `may_release`'tir.
- *`RoomSample`'a `detach_forced` sayacı:* gözlemlenebilirlik için doğru
  yer, ama metrik şemasına ek (sample alanı, toplayıcı, raporlayıcı) bu
  turun kapsamı dışı; bir kez uyarı + aktör sayacı şimdilik yeterli,
  açık iş olarak kalır. **Kapandı — küçük paket:** `detach_forced`
  (tavanın ezdiği her veto; iki `detach_expired_*` sayacının alt
  kümesi) iki aktörün `HoldEnd::Forced` kolundan örneğe, rapora,
  gsb-metric satırına ve Prometheus'a (`gsb_room_detach_forced_total`,
  OPS §3) gidiyor; loadgen fold'unda SUM. Uyarı hâlâ oda başına bir kez.
