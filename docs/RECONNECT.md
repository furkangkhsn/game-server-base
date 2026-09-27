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

### 3.3 Ayrılmanın nedeni: `DisconnectCause` (BACKLOG F27)

Politikaya üç uç ulaşır (§16, §16.3): taşıma öldü, girdi-boşta tavanı,
oyunun atması. Bugüne dek üçü de aynı `on_disconnect`'e NEDENSİZ
geliyordu; "düşeni park et, atılanı despawn et" diyen bir oyun bunu
kendi durumunda bayrakla taklit etmek zorundaydı. **Karar: aktör nedeni
söyler, kaderi yine oyun seçer.** Motor yalnız kendi ayırt ettiğini
adlandırır — hangisinin "rage quit", "AFK" ya da "ban" olduğu oyunun
okumasıdır.

| Çağrı noktası (oda ve shard aktöründe aynı) | `DisconnectCause` |
|---|---|
| `RoomControl::Detach` / `ShardMsg::Detach` — registry'nin `ConnClosed` yolu (dispatcher'ın `Close`'u ya da doğrudan DETACH) | `ConnectionClosed` |
| girdi-boşta tavanı (faz 0d), `afk_action` `leave_room` da `disconnect` da | `IdleInput` |
| oyunun atması (`TickCtx::kick` — oda faz 3b / shard faz 3c ya da tick sonu) | `Kicked` |

```rust
// gsb_core::room
#[non_exhaustive]
pub enum DisconnectCause { ConnectionClosed, IdleInput, Kicked }

// GameLogic — oda ve shard'ın ORTAK üst-trait'i (ShardLogic onu miras alır)
fn on_disconnect_with(&mut self, world: &mut W, player: PlayerId,
                      identity: &str, cause: DisconnectCause) -> Detach {
    self.on_disconnect(world, player, identity) // varsayılan: neden yok sayılır
}
```

- **Aktörler artık yalnız `on_disconnect_with`'i çağırır** (oda ve
  shard'ın `detach_player`'ı nedeni parametre alır; çağıran söyler).
  Varsayılanı eski kancayı çağırdığı için nedenden habersiz her mantık
  aynen çalışır — `on_join_as` → `on_join` deseni. `Detach`'ın kolları
  nedeni okumaz; motor her nedene aynı davranır (yalnız park'ın debug
  satırı nedeni yazar). Tel, `/metrics`, loadgen RESULT değişmedi.
- **`ShardLogic` için ayrı metot yok:** metot ortak üst-trait'te, iki
  aktör aynı metodu çağırır (Faz 3'ün "tek metot, iki aktör" ilkesi).
  Mantığı saran bir sarmalayıcı (kit'in sharded spatial/team
  kompozitleri) `on_disconnect_with`'i de iletmelidir; iletmezse
  varsayılan iç mantığın NEDENSİZ `on_disconnect`'ine düşer.
- **`ConnectionClosed` neyi kapsar.** Oda bağlantının NEDEN kapandığını
  öğrenmez — registry'nin `ConnClosed`'u neden taşımaz: eş gitti, bir
  taşıma koruması kapattı (idle timeout, write stall, ölü rUDP bandı,
  ihlal bütçesi, reddedilen akış), ya da bağlantıyı başka bir üyeliği
  için yargılayan bir sunucu hükmü düştü (aşağıda B43). İki son
  politikaya hiç ulaşmaz: aynı kimliğin yeni oturumunun eskisini
  ezmesi (registry bir LEAVE yollar → `on_leave`) ve odanın kapanışı
  (`on_shutdown`).
- **B43'ün yeni üyeliği `ConnectionClosed` ile biter** (§16.4): tavanın
  ya da atmanın hükmü, bağlantı yeniden katıldıktan sonra düşerse yeni
  üyeliği bağlantının kapanışı bitirir. O üyeliği tutan oda onu
  yargılamadı — hüküm önceki üyelikten (belki başka odadan) geldi —
  yani bu oda için gerçek, bağlantının kapanmasıdır. "Atma her yerde
  atmadır" diyen oyun yasak listesini kendisi tutar (§16.3: motor
  saklamaz).
- **`#[non_exhaustive]`** (DESIGN §5 ekleyici evrim): motor ileride daha
  çok ucu ayırt edebilir (ör. `ConnectionClosed`'ın arkasındaki hüküm);
  eşleşen mantık joker kol tutar, kırılmaz.

**Kit (yapı taşı, opt-in).** Kit odalarının kopma politikası oda
geneli kalır (`with_disconnect_policy`); `with_disconnect_policy_for(
cause, grace, to)` bir NEDEN için onu bütünüyle ezer — "atılan →
despawn, düşen → park" çekirdek kodu yazmadan:
`.with_disconnect_policy_for(DisconnectCause::Kicked,
Some(Duration::ZERO), ExpireTo::Despawn)`. Yedi kit odası da nedeni
yönlendirir (KIT-ARCHITECTURE §4.3 "F27"). Varsayılan değişmedi: ezme
yoksa her neden oda geneli kuralı alır.

**Elenen alternatifler.**

1. *`on_disconnect`'in imzasını değiştirmek* (nedeni dördüncü parametre
   ya da bir bağlam yapısı — `DisconnectCtx { player, identity, cause }`
   — olarak): her uygulayıcıyı kırar (çekirdek test mantıkları, yedi kit
   odası, demolar, oyunlar); bağlam yapısı ancak başka alanlar gelince
   kendini öder. Sağlanan metot aynı bilgiyi kırmadan verir ve trait'te
   zaten yerleşik bir desen (`on_join_as`).
2. *Nedeni ayrı bir bildirimle vermek* (`on_disconnect`'ten önce
   çağrılan `note_disconnect_cause`, ya da `TickCtx`'te bir alan):
   iki çağrı arasında mantıkta durum; `RoomControl::Detach` CONTROL
   fazında, tick bağlamı olmadan işlenir.
3. *Neden başına ayrı kancalar* (`on_kick`, `on_idle`): aynı karar için
   üç kanca; her sarmalayıcı üçünü iletir.
4. *`ConnClosed`'a bağlantının hükmünü (`ServerClose`) taşıyıp
   `ConnectionClosed`'ı alt nedenlere bölmek:* registry mesajı,
   dispatcher ve `RoomControl`/`ShardMsg::Detach` değişirdi; bilinen
   tüketici yok. `#[non_exhaustive]` kapıyı açık tutar.
5. *B43'te yeni üyeliğe `Kicked` demek:* hükmü registry üzerinden yeni
   üyeliğin odasına taşımayı gerektirir ve o oda yargılamadığı bir
   kararı uygulamış olur (yukarıda).

**Testler.** Çekirdek: `room/cause/tests.rs` — varsayılan metot her
nedeni eski kancaya, oyuncu + kimlik + cevap değişmeden iletir;
`room/tests/cause.rs` ve `shard/tests/cause.rs` — kapanan bağlantı →
`ConnectionClosed`, tavan her iki `afk_action` altında → `IdleInput`
(ardından gelen DETACH yeniden sormaz), atma → `Kicked`;
`tests/room_close/rejoin_races.rs` — B43'te ilk üyelik `IdleInput` /
`Kicked`, yeni üyelik `ConnectionClosed` (tek oda ve ızgara). Önce
yazıldı ve düştü (aktörler henüz `on_disconnect`'i çağırıyordu: hiçbir
neden kaydedilmedi). Mutasyon: altı çağrı noktasının her birinde nedeni
değiştirmek → kendi birim testi + B43 testi düşer; varsayılanı eski
kancayı çağırmayacak hale getirmek → varsayılan testi düşer. Kit:
`common/park/tests/cause.rs` — yedi oda türünde nedene göre politika
matrisi; `sharded/tests/team_actors/cause.rs` — gerçek aktörlerde (dört
shard aktörü + canlı registry, ve tek dünya oda aktörü) atılan
despawn olur (müttefik görmez, slot döner, aynı kimlik yeni varlık
alır), düşen park edilir (müttefik görür, aynı kimlik resume eder).

### 3.4 Transport ölümünün yolu ve düşen `Close` (BACKLOG B61)

Bağlantı kapanınca (`ConnClosed`) registry kaderi kendisi seçmez, yalnız
DETACH'ı yönlendirir. Bağlantının op dağıtıcısı (dispatcher) varsa
registry ona `try_send(RoomOp::Close)` yapar ve tek göndericiyi bırakır;
dağıtıcı önündeki op'ları (uçuştaki bir katılma dahil) sırayla işler, en
son hangi üyelikte kaldıysa onu `RoomControl::Detach` / yayın
`ShardMsg::Detach` ile odaya bildirir ve `DetachDone` raporlar. Dağıtıcı
yoksa registry tablodaki üyeliği `send_detach_direct` ile (spawn'lu
gönderim) bildirir. İki yolda da oyunun `on_disconnect`'i bir kez,
`DisconnectCause::ConnectionClosed` ile çalışır (§3.3).

**Sızıntı (B61).** `Close` kuyruğa girmezse (16'lık kuyruk dolu ya da
görev gitmiş; B57'den beri `close_ops_dropped` sayar) dağıtıcı eskiden
kuyruğunu boşaltıp detach'sız çıkıyordu: odadaki üye, registry satırı ve
ızgaranın üye yuvası oda bitene dek kalıyor, `on_disconnect` hiç
çalışmıyordu. Kuyruğa giremeyen op'lar aynı bağlantının tekrar
katılmalarıysa üyelik, tablonun henüz görmediği YENİ bir entity'dir.

**Karar.**

- *Kuyruk dolu:* dağıtıcı canlı ve önündeki op'ları işleyecek. Kuyruğunun
  kapanmasını (`recv` → `None`) `Close` sayar: döngüden çıkınca elindeki
  üyeliği detach eder, `DetachDone` ve `OpsClosed` yollar — `Close`
  kuyruğa girmiş olsaydı olacağın aynısı, aynı sırada. Önündeki bir
  katılma ÖNCE çalışır (sıra değişmez; kuyruğa girmiş `Close`'un arkasında
  da öyle çalışırdı), detach onun bıraktığı üyeliği bulur. Registry'nin
  göndericiyi bıraktığı her yer (bağlantı kapanışı, kayıtsız bağlantı
  kapanışı, shutdown) zaten "bağlantı bitti" demektir; başka bir anda
  kuyruk kapanmaz.
- *Görev gitmiş:* kuyruğu okuyacak kimse yok, üyelik bilgisi görevle
  gitti. Registry tablodaki üyeliği `send_detach_direct` ile kendisi
  bildirir (spawn'lu gönderim, S kuralı — registry oda posta kutusunu
  beklemez). Kayıtsız (satırı olmayan) bağlantıda bildirecek üyelik
  yoktur.
- *Çift detach yok:* dolu kuyrukta registry doğrudan detach YAPMAZ;
  gitmiş görevde dağıtıcı detach edemez. Yine de iki DETACH aynı odaya
  ulaşsa odanın muhafızı (bağ + entity + zaten park edilmiş satır)
  ikincisini sessizce yutar; politika bir kez sorulur.
- `close_ops_dropped` anlamını korur: "`Close` op'u kuyruğa girmedi".
  Geri düşüş onu sızıntı olmaktan çıkarır, sayılmamış yapmaz; ad
  yanıltıcı değil, aile değişmedi.

**Reddedilen:** dolu kuyrukta tablodan `send_detach_direct` (BACKLOG
satırının ilk önerisi). Tablo, dağıtıcının kuyruktaki katılmalarının
yaratacağı üyeliği henüz bilmez; doğrudan detach eski entity'yi hedefler,
oda onu (üst üste katılmanın sildiği) bayat detach olarak yutar ve son
üyelik yine sızar — mutasyonla gösterildi. Kuyruktaki katılmaları
kapanan bağlantı için atlamak da reddedildi: kuyruğa girmiş `Close`'un
yolundan ayrılır ve ızgaranın rezervasyonunu (`pending`) ayrıca
kapatmayı gerektirirdi.

**Erişilebilirlik.** Bağlantı aktörü her katılmanın yanıtını bekler ve
ayrılmayı yalnız tablo bir üyelik gösterirken yollar; tel üzerinden tek
bağlantı kuyruğu 16'ya dolduramaz, dağıtıcıda da panik yeri yok. B61 bu
yüzden gizli bir sızıntıydı (ham `RegistryMsg` üreticisi, gelecekte op'ları
boru hattına dizen bir yol ya da bir panik tetiklerdi); sayaç onu görünür
yaptı, düzeltme yolu kapatır.

**Testler.** `tests/room_close/close_op.rs` — oturmuş üyelikten sonra 40
katılma ve kapanış tek solukta: kuyruk 16'sını alır, `Close` reddedilir
(`close_ops_dropped` 1); son (17.) üyelik `on_disconnect`'i bir kez
`ConnectionClosed` ile görür, satır ve üye gider, tavanı 1 olan oda (tek
oda ve ızgara) yeni oyuncu alır. Önce yazıldı ve düştü (`on_disconnect`
hiç çalışmadı; satır oda 1'i 5 sn boyunca tuttu). `registry/actor/conns/
tests.rs` — gitmiş dağıtıcı: registry tablodan detach eder, oda
`DetachDespawned` raporlar, eski dağıtıcının geç kapanışı ikinci bir
`on_disconnect` doğurmaz. Mutasyonlar: kuyruk kapanınca detach'ı
kaldırmak → iki uçtan uca test düşer; gitmiş görevde doğrudan detach'ı
kaldırmak → birim testi düşer; dolu kuyrukta tablodan doğrudan detach
(reddedilen) → iki uçtan uca test düşer.

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
`GameLogic::on_disconnect` çağrılır (F27'den beri
`on_disconnect_with(.., DisconnectCause::IdleInput)`, varsayılanı
`on_disconnect`; §3.3) ve dönen `Detach` ne diyorsa o olur —
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
   öğrenir. Ayrı bir bildirim açılmayacak (bakımcı kararı, BACKLOG E9,
   2026-09-27): sunucunun başlattığı çıkarmanın istemciye söylenmesi
   gereken biçimi bağlantıyı KAPATMAKTIR (`afk_action = disconnect`, E8'in
   oyun fiili) — `ERROR 9` + kapanış. `leave_room` onu bilerek seçen
   dağıtımın sessiz yoludur.

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
varlığın kaderini seçer; `Disconnect` yalnız TAŞIMAYI ekler. Politikanın
gördüğü neden ikisinde de `IdleInput`'tur (§3.3): eylem bağlantının
kaderini seçer, varlığınkini değil. Park/bot
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
  bırakılır. `parked`, odanın isteği GÖNDERDİĞİ andaki durumudur (B41,
  aşağıda): parkın raporu istekten önce varamaz.
- `parked` değil → aidiyet gider (sharded üye sayısı düşer, `leaves`
  sayılır); taşıması ZATEN ölmüş satır (önce `ConnClosed` geldiyse)
  doğrudan silinir — onu başka hiçbir şey bırakmazdı.
- İsteğin üyeliği satırın şimdiki üyeliği değil (`room` VE `entity`
  tutmuyor: bağlantı istek beklerken ayrıldı, başka yere ya da yeni
  varlıkla katıldı) → tablo OLDUĞU GİBİ kalır, ama hüküm yine bağlantıya
  gider (B43, §16.4): hüküm bağlantınındır, yerleşim üyeliğin. Satır yok
  (bağlantı kapandı, satır bırakıldı) → sessiz no-op.

Böylece istek, bağlantının `ConnClosed`'u ve odanın `DetachDespawned`'ı
hangi sırada gelirse gelsin aynı son duruma varılır.

**Bekleyen istekte park gönderimde yeniden sınanır (B41).** Registry,
satır henüz `detached` değilken gelen bir `DetachDespawned`'ı bayat bir
yankı sayıp düşürür (resume'un yeniden bağladığı satırı korumak için).
Dolu posta kutusunun arkasında `parked` diye bekleyen bir istek bu
yüzden yanlışa düşerdi: park beklerken despawn ile biterse (kısa ya da
sıfır grace) parkın raporu 0c fazında, bekleyen isteğin 0d gönderiminden
ÖNCE yola çıkar; registry onu düşürür, ardından gelen `parked` istek
satırı `detached` işaretler ve onu bırakacak hiçbir olay kalmaz
(bağlantının kapanışının yönlendirdiği DETACH'ı oda yok sayar: binding
gitti). Satır `max_connections` slotunu — ızgarada `ShardGroup` üye
slotunu — aynı kimlik o odaya dönene ya da oda bitene dek tutardı ve
`RoomStatus` onu üye sayardı. Deterministik olarak kuruldu: tek slotluk,
dolu bir registry posta kutusu (testin elindeki alıcı adım adım
boşaltılır), sıfır grace'li park; registry'ye varış sırası
`DetachDespawned`, `CloseConn(parked=true)` idi
(`room/tests/idle/afk/races.rs`, shard'da `shard/tests/idle/afk.rs`;
registry'deki sonucu `tests/room_close/close_races.rs` sabitler).

Kural (`reconcile_closes`, oda ve shard): her gönderim denemesinden önce
bekleyen istek, oda bağlantının bir üyeliğini hâlâ tutuyorsa `parked`
kalır — park, onu devralan bot (slot hâlâ dolu) ya da açık bağlantının
yeniden aldığı üyelik (resume/rejoin; onu bağlantının kendi kapanışı
taşıma-ölümü yolundan yerleştirir). Tutmuyorsa `parked = false`: istek
despawn olarak yerleşir. İki varış sırası da aynı sona varır — rapor
önce: no-op, istek aidiyeti bırakır; istek önce: aidiyet gider, rapor
bayat. Bayrak yalnız `true`'dan `false`'a döner (despawn geri alınamaz).
Shard'da beklerken komşuya göç eden park burada bitmiş görünür: istek
despawn yerleşir ve park yaşarken registry onu saymaz — eksik sayım,
sızıntı değil; park bitince ya da resume edilince kendini onarır (göç
eden parkın raporunu komşu shard yollar, ayrı bir gönderici: hiçbir
oda-içi sıra onu isteğin arkasına koyamaz).

Elenenler: (1) *B40'ın "rapor önünde bekleme" kuralını kapatma isteğine
de uygulamak* — tek başına yetmez: rapor kuyrukta beklemez, 0c'de gider
ve 0d'deki isteğin önüne zaten geçmiştir; üstelik `Disconnect` + despawn
kolu kendi raporunu aynı 0d'de kuyruğa koyduğundan her kapatmayı bir
tick geciktirirdi. Gönderimde sınama ile sıra önemsizleşir. (2) *Oda
başına tek sıralı giden kutusu* (rapor, kapatma, ayrılma tek FIFO, ilk
DOLU'da durur) — oda-içi nedensel sırayı korur ve bu yarışı kapatır;
ama B40'ın başka bir oturumun resume'u yarışı (`SpawnDone` başka
göndericiden gelir) için gönderimde sınama yine gerekir, shard göçünde
rapor komşudan geldiği için sızıntı geri gelirdi ve iki aktörde üç
kuyruk ile testleri değişirdi. (3) *Registry'de düzeltme* (raporun canlı
satırı da bırakması) — rapor, kapatılmayı bekleyen bağlantının satırını
silerdi; ardından gelen istek satır bulamaz, soketi açık bırakırdı.

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

**Oyun mantığına açılmadı (bilinçli) — E8'de açıldı (§16.3).** Fiil
E6 turunda yalnız tavanın eylemiydi. Genel bir "oyuncuyu at" (ör.
`TickCtx`/bir kanca üzerinden) `GameLogic` + `ShardLogic` + kitin
`Game`'ine yeni yüzey ve yeni anlambilim kararları getirirdi (üyelik
hangi yoldan biter — `on_leave` mi `on_disconnect` mi, gerekçe metni
kimin, hız sınırı) — BACKLOG'a ayrı madde (E8) olarak yazıldı. İç fiil
(`CloseRequest`, sebep + metin taşıyan) buna hazırdı: E8 yeni bir
`ServerClose` etiketi (`Kicked`) ve bir üretici (`TickCtx::kick`)
ekledi.

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

E6'nın kapatma isteği bu kurallardan yalnız 2.'yi paylaşır (§16.1, B41 —
`parked` gönderimde sınanır). 1. kurala gerek yoktur: sınanan bayrakla
rapor ile isteğin iki varış sırası da aynı sona varır, ve rapor 0c'de
gittiği için bekleme tek başına sırayı zaten kurtaramazdı. 3. kurala da
gerek yoktur: bağlantının yeni üyeliğinin TABLOSUNU `room` VE `entity`
koruması korur, hüküm ise yine de bağlantıya gider (B43, §16.4) —
taze katılmayla yeni varlıkta da, aynı varlığı sürdüren bir resume'da
da bağlantı kapanır (dağıtımın istediği kapanış) ve yeni üyelik
bağlantının kapanışıyla taşıma-ölümü yolundan biter. *(Düzeltme notu:
bu paragraf B43'ten önce "taze bir katılma isteği bayat bırakır, soket
açık kalır" diyordu — açık yarışın tarifiydi.)*

**Registry: `RegistryMsg::LeaveConn`.** Tek tablo araması, E6'nın
yerleşim koruması (`room` VE `entity` satırın şimdiki üyeliği değilse
no-op — burada bütün istek no-op'tur: teslim edilecek bir hüküm yok ve
`LeftRoom` bildirimi yeni bir üyeliğe ulaşmamalı):

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

### 16.3 Oyunun atma fiili: `TickCtx::kick` (E8)

**Karar (bakımcı, 2026-09-27): atma = bağlantıyı kapatmak.** Oyun
mantığı bir üyeyi SUNUCUDAN atar: üyelik §16'nın yolundan biter
(`on_disconnect` — varlığın kaderini oyunun `Detach`'ı seçer: park / AI
devri / despawn), sonra soket E6'nın fiiliyle (`CloseRequest` →
registry → bağlantı) kapanır. `on_leave` yolu ve "odadan çıkarıldı ama
bağlı" durumu YOK (E9 kapandı). Yeni tel öğesi yok, base protokol sürümü
değişmedi. Yol, `afk_action = disconnect` altındaki tavanın yolunun
aynısıdır; yalnız tetikleyici (oyun) ve sayaç etiketi farklıdır.

**Yüzey (tek fiil, üç kapı).**

```rust
// gsb_core::room
pub struct TickCtx<'a> { …, pub idle: IdleView<'a>, pub kicks: Kicks<'a> }
impl TickCtx<'_> {
    pub fn kick(&self, player: PlayerId, reason: impl Into<String>);
}
pub struct Kicks<'a>;               // Copy; Default = etkisiz (el yapımı bağlam)
impl Kicks<'_> { pub fn kick(&self, player: PlayerId, reason: impl Into<String>); }
pub struct KickQueue;               // aktörün tick başına kuyruğu; testler de kurar
impl KickQueue { pub fn kicks(&self) -> Kicks<'_>; pub fn take(&self) -> Vec<Kick>; }
pub struct Kick { pub player: PlayerId, pub reason: String }
pub const KICK_REASON_MAX_BYTES: usize = 256;
pub fn kick_message(reason: &str) -> String;   // "kicked: <reason>" | "kicked"

// gsb_kit::game
pub fn kick(world: &mut World, entity: Entity, reason: impl Into<String>);
```

- **`GameLogic` / `ShardLogic`:** yeni METOT yok — fiil her tick
  kancasının zaten aldığı bağlamda (`ctx.kick`), `ctx.since_input`'un
  deseni (§16). `ingest`, `handle_request`, `update`, shard'ın
  `ingest_seam`/`update_seam`'i, ve yayın fazının kancaları
  (`snapshot`, `keepalive`, `team_exchange`) sorabilir.
- **Kit `Game`:** kit oyunları varlıkla düşünür; `gsb_kit::game::kick(
  world, entity, reason)` dünyayı değiştirebilen HER kancadan (`ingest`,
  `systems`, `handle_request`, `may_release`, shard'da
  `apply_remote_effect` ve seam kancaları) sorulabilir. İstek bir dünya
  kaynağında bekler (yalnız ilk atmada eklenir: hiç atmayan oyunun
  dünyasında kaynak yoktur); yedi kit odası oyunun sistemlerinden hemen
  sonra (`update`'in sonunda) sahibi çözüp çekirdeğin fiiline iletir —
  tek dünya odaları oyuncu→varlık tablosuyla, sharded odalar
  varlık→oyuncu tablosuyla (MIGRATE'ten önce). Sahibi olmayan varlık
  (NPC, gitmiş) yok sayılır. `PlayerId`'yi elinde tutan kanca doğrudan
  `ctx.kick` da çağırabilir.

**Ne zaman uygulanır (isteyen kancanın içinde ASLA).** Fiil yalnız
kuyruğa yazar (bağlamın ödünç verdiği bir `Cell`; await yok, kancaya
yeniden giriş yok). Aktör kuyruğu, sorabilecek kancalar döndükten sonra
iki noktada uygular:

| Soran kanca | Oda | Shard |
|---|---|---|
| `ingest`, `handle_request`, `update` (shard: `*_seam`) | faz 3b — SYSTEMS'tan sonra, BROADCAST'tan önce | faz 3c — EFFECTS OUT'tan sonra, **MIGRATE'ten önce** |
| `snapshot`, `keepalive` (shard: + `team_exchange`) | tick sonu (BROADCAST'tan sonra) | tick sonu (BROADCAST'tan sonra) |

`on_disconnect` bu noktada çağrılır (neden `Kicked` — §3.3). İstek ile uygulama arasında
CONTROL fazı koşmaz: atma, isteyen kancanın gördüğü üyeliği yargılar
(oyuncunun aynı pencerede gönderdiği `LEAVE`, CONTROL'de SONRA işlenir
ve üye olmayana düşer). Birinci noktada uygulanan atmada üye o tick'in
yayınını almaz. Aktör input-idle saatini uygulamadan önce geri alır
(ayrılma yolu üyeyi saatten çıkarır) ve yayın için yeniden ödünç verir.
Kapatma isteği E6'nın kuyruğuna girer ve **sonraki tick'in 0d fazında**
gider (`flush_close_requests`, B41'in `parked` yeniden sınaması dahil);
registry satırı yerleştirir, bağlantıya `ConnIn::ServerClosed { Kicked }`
iletir.

**Yol, E6'nın `Disconnect` kolunun aynısı.** Canlı üye (satırı var,
`detached` değil): `detach_player(…, report = true)` → park edildiyse
satırın giden yarısı bırakılır (`release_outbound`; yoksa soket
kapanamaz) → `CloseRequest { cause: Kicked, reason: "kicked: …",
parked }`. Registry yoksa (bağımsız oda, test düzenekleri) üyelik yine
politikayla biter, istek kuyruğa girmez.

**Gerekçe metni: sınır ve önek.** Oyunun metni **256 bayta**
(`KICK_REASON_MAX_BYTES`) `char` sınırında kesilir — sorulduğu anda,
yani kuyruk da çerçeve de küçük kalır; çok baytlı bir karakter asla
bölünmez. İleti `kicked: <reason>` (boş gerekçede yalnız `kicked` —
`Error.message` her zaman doludur). Gerekçe: motorun kendi kod-9
metinleri tek satırlık, ~100 baytlık, hükmü önekle adlandıran metinlerdir
(`input idle: …`, `stream rejected: …`); önek istemcinin oyunun atmasını
motorun kapanışlarından ayırmasını sağlar, 8 + 256 bayt çerçeveyi tek
parçasız rUDP datagramında tutar.

**Tel ve sayaç.** En-iyi-çaba, beklemesiz `ERROR 9` (bağlantı aktörü
`IdleInput` ile aynı kolda: `try_notice`) — atılan istemci okumayı
bırakmış olabilir (oyun onu çoğu zaman tam bu yüzden atar); beklemeli
gönderim aktörü write-stall penceresine kadar park ettirirdi. Sonra
kapanış. `server_closes{reason="kicked"}` — yeni etiket SONA eklendi
(Prometheus ve OTLP'de tek satır; log satırında `server_close_kicked=`;
loadgen metrik teli GSMG; RESULT'ta her sebep için bir anahtar kuralıyla
`server_close_kicked=`).

**Kenar durumları.**

| Durum | Kural |
|---|---|
| Bilinmeyen oyuncu, zaten ayrılmış/despawn olmuş, park edilmiş (`detached`), bot beslenen | no-op: politika çağrılmaz, istek yok. **Sayılmaz** (yalnız `debug` log): canlı olmayan üyeyi atmak oyunun kendi yarışıdır, operatörün yapacağı bir şey yoktur; yeni sayaç `/metrics`'i değiştirirdi. Oyun isterse kendi sayacını (F9) tutar |
| Aynı oyuncuyu aynı tick'te iki kez atmak | bir politika çağrısı, bir kapanış, İLK gerekçe: ilk uygulama satırı bitirir (ya da park eder), ikincisi canlı üye bulamaz |
| Shard'da aynı tick göç edecek üye | faz 3c MIGRATE'ten önce uygulanır: despawn edilen üye göçmez; park edilen varlık PARK olarak göçer (bayrakları taşınır, §14.2) ve kapatma isteği gider — bekleyen isteğin `parked`'ı gönderimde `false`'a döner (B41'in bilinen eksik sayımı, sızıntı değil) |
| Shard'da MIGRATE'ten SONRA soran kanca (`team_exchange`, `snapshot`, `keepalive`) bu tick göçmüş üyeyi atar | no-op: shard kanca sorduğunda üyeye artık sahip değildi (komşuya iletme yok — kit oyunlarının MIGRATE sonrası bağlam kancası yok, ham `ShardLogic` için tanımlı bir no-op) |
| Registry posta kutusu dolu | E6'nın kuralı: istek kuyrukta kalır, sonraki tick'te yeniden denenir; despawn kolunun raporu (0c) aynı kutuyu önce kullanır |
| Registry kapalı (süreç iniyor) | istek düşer (E6) |

**B43 ile etkileşim (düzeltildi, §16.4).** Registry doygunken atılan
istemci, istek beklerken yeniden katılabiliyordu (despawn edilmiş üyenin
aksiyon kanalı kapalıdır, bağlantı kendini ayırıp doğrudan `JOIN`
gönderebilir): istek `room`+`entity` korumasında bayat kalıyor, soket
açık kalıyor, atılan istemci atmadan kurtuluyordu. Artık hüküm
bağlantıya yine gider: yeni üyelik oyunun `on_join`'inden geçmiş olabilir,
ama bağlantının kapanışıyla `on_disconnect`'ten bir kez geçerek biter.

**Kit'in varsayılan kaderi.** Kit odalarının varsayılan politikası
kimliği olan oturumu PARK eder (`DEFAULT_DISCONNECT_GRACE`, sonra bot);
atılan oyuncu aynı kimlikle dönerse süre içinde resume eder. Atılan
oyuncuyu tutmaması gereken oda `with_disconnect_policy(Some(Duration::ZERO),
…)` ile kurulur (hemen despawn — her neden için). Kaderi ATMAYA özgü
seçmek (düşeni park, atılanı despawn) F27'den beri bir yapı taşıdır:
`with_disconnect_policy_for(DisconnectCause::Kicked, Some(Duration::ZERO),
ExpireTo::Despawn)`; ham `GameLogic` aynı şeyi `on_disconnect_with`'in
`cause`'una bakarak yapar (§3.3).

**Varsayılan değişmedi.** Hiç atmayan bir oyunda davranış ve baytlar
aynıdır: kuyruk tick başına yerel, boş ve ayırmasız; uygulama boş listede
bir uzunluk testi; kit tarafında kaynak hiç eklenmez. `/metrics` yalnız
`kicked` satırıyla değişti.

**Tur sırasında bulunan hata (düzeltildi).** Shard tick gövdesi
input-idle saatini oyun kancalarına ödünç verip ancak BROADCAST'tan sonra
geri alıyordu; MIGRATE boş yer tutucu saatle koşuyordu: göçen üyenin
damgası (`PlayerMigration.last_input`) hep `None` gidiyor, alıcı shard
saati hiç başlatmıyordu — sınır geçişi boşta oyuncuyu tavandan kalıcı
olarak çıkarıyordu (gönderen de bayat bir yuva tutuyordu). Gövde saati
artık MIGRATE'ten önce geri alıyor ve TEAMS + BROADCAST için yeniden
ödünç veriyor (`shard/tests/idle/migrate.rs`). Atma'nın faz 3c noktası
aynı yeniden yapılanmayı kullanır.

**Elenen alternatifler.**

1. *`GameLogic`'e toplama metodu* (`take_kicks(&mut self, out)`, mantık
   kendi kuyruğunu tutar, aktör tick sonunda sorar — `logic_counters`
   deseni): her oyun bir kuyruk taşırdı, kit'te her oda ayrıca
   iletirdi; bağlam zaten her kancanın per-tick dikişidir (`since_input`).
2. *Anında uygulamak* (kancanın içinde `on_disconnect`): isteyen kancaya
   yeniden giriş; `&mut self` çakışması.
3. *Sonraki tick'in 0d fazında uygulamak* (tavanın noktası): arada
   CONTROL koşar — oyuncunun kendi `LEAVE`'i atmayı yutar, soket açık
   kalır; shard'da göç arada kalır, atmanın komşuya taşınması gerekirdi.
4. *Tek nokta, tick sonu:* shard'da üye aynı tick MIGRATE'te göçer;
   atmayı `PlayerMigration`'a ya da yeni bir `ShardMsg`'a bindirmek
   ertelenmiş göç ve geri alma yollarıyla karmaşık. MIGRATE'ten önce
   uygulamak yarışı yapısal olarak kaldırır.
5. *MIGRATE sonrası sorulan atmayı komşuya iletmek:* yeni `ShardMsg`,
   nadir bir durum için; tanımlı no-op seçildi.
6. *`RefCell`:* çift ödünçte panik riski; `Cell` (al/koy) panik yok.
7. *Atma için yeni `ErrorCode`:* kod 9 "sunucunun oturum hakkındaki
   hükmü" sınıfıdır (§16.1'in gerekçesi), istemciye yeni bir karar
   taşımaz.
8. *No-op'lar için sayaç* (`kicks_ignored`): yukarıda; `/metrics`
   kapısı.
9. *Kit'te atmaya özgü kader* (atılan → despawn): politika kararı ve
   yeni yüzey; açık iş olarak not edildi — F27'de opt-in yapı taşı
   olarak açıldı (§3.3).

### 16.4 Hüküm bağlantınındır, yerleşim üyeliğin (B43)

**Yarış.** Oda üyeliği bitirdi (`afk_action = disconnect` altında tavan,
ya da oyunun atması) ve varlığı despawn etti; kapatma isteği kuyruğa
girdi, ama registry posta kutusu DOLU olduğu için `try_send` her tick
reddediliyor (§16.1: düşürülmez, yeniden denenir). Bu arada istemcinin
sonraki oyun karesi kapalı aksiyon kanalına çarpar, bağlantı kendini
sessizce ayırır; bir sonraki kareye `ERROR 6` gelir, istemci `JOIN`
gönderir. Bağlantının gönderimi *beklemeli*dir (`send().await`), yani
dolu kutuda sıraya girer ve odanın `try_send`'inden önce yerleşir: yeni
üyelik, YENİ varlık. Geç gelen istek artık satırın üyeliğini
adlandırmaz; eskiden bayat sayılıp düşerdi — soket açık kalır, atılan
istemci atmadan kurtulurdu. (Park kolunda bu yol yoktur — park edilen
satır aksiyon kanalını tutar, bağlantı kendini odada sanır — ama
`LEAVE_ROOM_REQ` + `JOIN` aynı kaçışı her iki kolda da açıyordu.)

**Koruma neyi koruyordu.** Önce: `ConnectionId` süreç ömrü boyunca
tekildir — kabul döngülerinin paylaştığı tek atomik sayaç (`ConnIdSeq`,
1'den yoğun, geri dönmez); park anahtarları ayrılmış üst biti taşır ve
bir kapatma isteğine konu olmaz (istek yalnız CANLI üyeye çıkar, park
satırı canlı değildir). Yani bir istek yalnız kendi bağlantısını
adlandırabilir. `room`+`entity` koruması iki işi birlikte yapıyordu:

1. **Tablo yerleşimi** — `settle_ended` ya da `detached` işareti yalnız
   isteğin bitirdiği üyeliğe uygulanmalı. Sonraki bir üyeliğe uygulansa
   canlı bir aidiyeti siler ya da canlı satırı `detached` işaretler; eski
   üyeliğin sonu ise zaten yerleşmiştir (aşağıda) — ikinci yerleşim çift
   sayım olur (ızgarada üye sayısı iki kez düşer). **Bu iş kalır.**
2. **Hükmün teslimi** — korunacak bir şey değildi. Gerçek bir istek her
   zaman bağlantının SAHİP OLDUĞU bir üyeliği adlandırır: oda yalnız o an
   tuttuğu bir üyeyi yargılar. Bağlantının "hükümden önce meşru olarak
   ayrılıp yeniden katılması" odanın sırasında yoktur: aynı pencerede
   gönderilen `LEAVE` odada hükümden SONRA işlenir (§16.3: atma,
   isteyen kancanın gördüğü üyeliği yargılar). Hüküm üyeliği değil
   bağlantıyı yargıladı; bağlantı hâlâ açıksa hüküm ona düşer.

B41'in `parked` yeniden sınaması yalnız eşleşen kolda anlam taşır ve
değişmedi; eşleşmeyen kol `parked`'ı okumaz. Park anahtarıyla anahtarlı
satırlara (B40) eşleşmeyen kol hiç dokunmaz.

**Karar.** Registry'nin `on_close_conn`'u:

- Satır yok → no-op (değişmedi).
- Satırın şimdiki üyeliği isteğinki → yerleşim + hüküm (değişmedi).
- Değil → **tablo olduğu gibi kalır, hüküm yine iletilir**
  (`ConnIn::ServerClosed { cause, reason }`, spawn'lu gönderim, S kuralı;
  taşıması ölmüş satırın inbox'u yoktur → hiçbir şey).

Eski üyeliğin sonu her biçimde zaten yerleşmiştir: aynı odaya taze
katılmada `SpawnDone` satırı hâlâ bağlı bulur ve yeni üyelik eskisinin
slotunu devralır (`fresh = false`, üye sayısı değişmez); başka odaya
katılmada `SpawnDone`'un "bildirilmemiş bitiş" kuralı eski slotu geri
verdi (§16.2); ayrılmada `LeaveDone`/`direct_leave` yerleştirdi. Yeni
üyelik ise her kapanan bağlantının üyeliği gibi biter: `ConnClosed` →
dispatcher'ın `Close`'u (ya da doğrudan DETACH) → oyunun
`on_disconnect`'i bir kez → despawn'da `DetachDespawned` satırı ve slotu
bırakır, park'ta §4'ün olağan satırı kalır. Kararı yine oyunun politikası
verir (kit varsayılanı kimlikli oturumu park eder, §16.3 "Kit'in
varsayılan kaderi"); politikanın gördüğü neden `ConnectionClosed`'dır —
yeni üyeliği tutan oda onu yargılamadı (F27, §3.3).

Oda ve shard tarafı DEĞİŞMEDİ (kuyruk, `try_send`, Full/Closed kuralı,
B41 sınaması); tel aynı (`ERROR 9` + kapanış, E6/E8'in baytları);
`/metrics` aynı, yeni sayaç yok — hüküm bağlantı kapanırken bir kez
kaydedilir (`server_closes{reason}`). **Kabul edilen:** aynı odaya
doğrudan yeniden katılmada eski üyeliğin sonu registry'nin kümülatif
`leaves`'inde sayılmaz (yeniden katılma `joins`'te yeniden sayılır —
mevcut anlambilim; B40'ın 3. kuralında da böyle). Registry bu geçmişi
`LEAVE` + `JOIN`'den ayıramaz; saymak ikincisinde çift sayardı. Sızıntı
değil, kümülatif bir sayaçta bir eksik.

**Elenen alternatifler.**

1. *Odanın bağlantıya doğrudan söylemesi* (bağlantı join kabul etmeyi
   bıraksın, registry sonra öğrensin): oda yalnız üyenin giden çerçeve
   kuyruğunu (writer pump) tutar, bağlantı aktörünün inbox'unu değil —
   yeni bir tutamaç Join/Seat'e, oda ve shard satırına, göç yüküne
   (`PlayerMigration`) ve bağlantıya yeni bir "kapanıyorum" durumu
   gerekirdi; registry satırı yine §16.1'in yolundan yerleşmeli (iki
   yol). Üstelik kapatmaz: bildirim B12 gereği en-iyi-çaba `try_send`'dir,
   dolu inbox'ta düşer ve kaçış geri gelir.
2. *Bekleyen isteği beklemeli göndermek* (spawn'lu `send().await`):
   dolu kutunun FIFO bekleyenlerine girer, ama bağlantının `JOIN`'i daha
   önce girmiş olabilir — sıra yine yarış; üstelik sınırsız spawn.
3. *Odanın, bekleyen isteği olan bağlantının join'ini reddetmesi ya da
   isteği yeni varlığa yeniden hedeflemesi:* yalnız aynı odayı kapsar;
   başka odaya katılma ve `LEAVE` + `JOIN` kaçışı kalır; yarışta yeni bir
   ret teli.
4. *Registry'nin "bildirilmemiş bitişi" (bağlı satırdan gelen join)
   görünce join'i reddetmesi/ertelemesi:* aynı işaret B40'ın meşru
   yeniden katılmasında da görülür (3. kural); `LEAVE` + `JOIN`'de işaret
   hiç yok.
5. *Biten üyelikleri satırda tutup yalnız onlardan birini adlandıran
   isteği teslim etmek* (uydurma isteği no-op tutmak için): tek yuvalı
   biçim çok adımlı kaçışa açık (doymuş kutuda `try_send` bekleyen
   göndericilere her tick kaybeder; istemci `JOIN`, `LEAVE`, `JOIN`
   yapabilir), küme biçimi sınırsız. Gerçek bir istek zaten yalnız
   bağlantının kendi üyeliğini adlandırabildiğinden hiçbir şey kazandırmaz.

**Testler.** `tests/room_close/rejoin_races.rs` (düzenek:
`rejoin_rig.rs`) — canlı registry, gerçek bağlantı aktörleri, paused
saat. Odaların registry'ye giden yolu bir röleden geçer; röle her mesajı
sırasıyla iletir, yalnız kapatma isteklerini TUTAR ve teste verir: oda
için dolu kalan posta kutusunun deterministik eşdeğeri (registry'ye
varış sırası aynıdır — önce join'in `SpawnDone`'u, sonra istek). Mantık
her join'de yeni varlık basar. Tavan (`disconnect`) ve atma, tek oda ve
ızgara (tek slot): istemci `ERROR 6` alır, yeni varlıkla katılır, istek
sonra teslim edilir. Düzeltmeden önce iki test de düştü: bağlantı
500 ms içinde (yeni üyeliğin kendi tavanından önce) kapanmadı. Sonra:
tek `ERROR 9` (gerekçesiyle), kapanış, hüküm `idle_input`/`kicked`;
kancalar her üyelik için bir kez (`on_join`, `on_disconnect`, despawn'ın
`on_leave`'i); üye 0, registry tablosu boş, tek slot yeni oyuncuyu
alır. `tests/room_close/registry.rs`'de eski kuralı (bayat istek:
bağlantıya hiçbir şey) sabitleyen test yeni kuralı sabitler: başka
varlık ya da başka oda adlandıran istek hiçbir şey yerleştirmez ve
bağlantı yine söylenir; bağlantının kendi ayrılmasından (`LEAVE`) sonra
gelen istek de kapatır. Mutasyon: eşleşmeyen kolda teslimi kaldırmak →
üç test düşer; teslimi yalnız bir odada olan satıra kısmak → birim testi
düşer (ayrılmadan sonra); eşleşmeyen kolda yine yerleştirmek → birim
testi düşer (üye 1 → 0). Uçtan uca testler ikinci mutasyonu görmez (aynı odaya
yeniden katılmada dispatcher'ın DETACH'ı sonucu aynı yere taşır); kuralın
sahibi birim testidir.

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
