# gsb: Olaylar (sistemin bize ittiği) — Tasarım (tur e0)

> **Durum:** TASARIM — kullanıcı incelemesi bekliyor. Uygulama YOK; bu
> turun çıktısı bu doküman ve `spikes/e0-events` (gsb çalışma alanının
> üyesi olmayan, motoru değiştirmeyen bir kanıt crate'i). İstek sisteminin
> (`docs/SERVICES.md`, s0–q6, uygulandı) ikizidir; SERVICES §15'teki s0
> metninin yerini alır. §2'deki kararlar kullanıcıyla alındı ve yeniden
> açılmaz; açık kalanlar §20'de numaralı sorular olarak, her biri
> öneriyle.
>
> **e1 uygulandı (2026-10-08):** sözleşme ve tel, yönlendirme yok —
> `gsb_protocol::event` (`Event`, `WorldEvent`, `ClientEvent`, `Dest`);
> `gsb/options.proto`'da `Destination`, `EventOptions`, `(gsb.event)`
> 50004, `(gsb.reserved_ops)` 50005; `gsb-build` olay planı, üretimi ve
> isteklerle ortak tek op uzayı denetimleri; `base.proto`'da `Push`
> (`stream = 5` dahil, §20 E5), `Auth.features = 4` + `Feature` enum'u
> (`FEATURE_PUSH = 1`), `op::base::PUSH = 17`; `PROTOCOL_VERSION`
> değişmedi (eklemeli). Demo örneği `ItemGranted` (1007, BOTH). Ayrıntı
> ve sapmalar §21 "e1 — uygulanan biçim". §20'deki öneriler kullanıcıca
> kabul edildi.
>
> **e2 uygulandı (2026-10-08):** çekirdek yönlendirme — `gsb_core::events`
> (`Events` tablosu, `Publisher`/`Publishers`, `Push`, `PushRefused` +
> `TooLarge`), registry'de yönlendirme tablosu (`EventRoutes`) +
> `RegistryMsg::Push`, `Authed { identity, features }`, odanın ve her
> shard'ın kendi olay kutusu (`RoomConfig::event_capacity`) ve EVENTS
> fazı, `ConnIn::Push` → `PUSH` karesi, `GameLogic::{boot_events,
> update_with_events}`, `ShardLogic::update_seam_with_events`,
> `RegistryParts::publisher`, sayaçlar → metrikler. Shard göçünün çiti
> e3. Ayrıntı ve sapmalar §21 "e2 — uygulanan biçim".
>
> Kod ve yorumlar İngilizce'dir; bu doküman Türkçe'dir (repo sözleşmesi).

**Tek paragraf özet.** Olay, sunucu kodunun (bir servis, bir admin görevi,
bir oda) **bir kimliğe, bir odaya ya da herkese** ittiği tipli bir
mesajdır. Hedefi tipin kendisindedir ve `.proto`'da bildirilir:
`message FriendOnline { option (gsb.event) = { op: 2003, to: CLIENT }; … }`
— `WORLD` (oyuncunun odasının mantığı tepki verir: bakiye), `CLIENT`
(doğrudan istemciye; hiçbir oda görmez: sohbet) ya da `BOTH`. `gsb-build`
her olay için `impl Event` (+ `WorldEvent`/`ClientEvent` işaretçisi) üretir;
yanlış tarafta kullanmak derleme hatasıdır. Yayıncı `Publisher` tutamacıyla
iter (`push`, `push_many`, `push_room`, `push_all`; tek `try_send`, yük bir
kez kodlanır); **yönlendirici** (registry'nin kimlik → konum tablosu) her
alıcının her bacağını (CLIENT/WORLD) ayrı ayrı numaralar ve teslim eder:
CLIENT bacağı bağlantı aktörüne, oradan `PUSH` karesiyle istemciye; WORLD
bacağı odanın **kendi** sınırlı olay kutusuna. Oda olayları **EVENTS**
fazında (RESPONSES'tan sonra, READ'den önce) tabloya alır; sistemler
`events.take::<E>()` ile tipli çeker; kimsenin almadığı tick sonunda
**atılır ve sayılır**. Teslim en-fazla-bir-kez, her kayıp kendi adıyla
sayılır, (yayıncı, kimlik, bacak) başına sıra numarası istemcinin boşluğu
görmesini sağlar. Shard göçünde mezar taşı + ileri gönderme + **çit**
(seal/fence) yayıncı → kimlik FIFO'sunu korur. Olaylar odanın kayıtlı
girdileridir: tekrar oynatma aynı hash'leri verir.

```text
CONTROL → RESPONSES → EVENTS → READ → CONVERT → SYSTEMS → FLUSH → BROADCAST
```

---

## 1. Problem ve s0'ın kusurları

İstek sistemi "oyuncu sorar, sunucu cevaplar"ı çözdü. Eksik yarı: **kimse
sormadan** sunucunun oyuncuya söylemesi — "bob sana 100 altın gönderdi",
sohbet satırı, "arkadaşın çevrimiçi", "web mağazasından aldığın paket
geldi". Bugün bunun tek yolu oyunun kendi snapshot'ıdır: bir odanın
dışındaki bir şey (servis, başka oda, web arka ucu) bir oyuncuya ulaşamaz.

s0'ın olay metni (SERVICES §15, bu turda yeniden yazıldı) üç yerde ağırdı:

1. **WORLD itmesi üyenin girdi kanalından gidiyordu.** Gerekçe yapısaldı
   (kanal oturumu göçte ve devamda izler), bedeli ise kullanıcının
   reddettiği şey: kendi girdi kanalını dolduran (spam yapan) istemci
   kendi itmelerini düşürüyordu; itmeler istemcinin girdi bütçesini
   paylaşıyordu.
2. **Hub'a düşüş** (odası olmayan oyuncu): s0b hub'ı kaldırdı.
3. **`on_events` kancası + `PushIn::decode`**: istek tarafının
   kaldırdığı tören (tipsiz liste, her oyun kendi eşlemesi).

Bu tur üçünü de kaldırır ve istek sisteminin ilkelerini aynen taşır.

## 2. Kararlar (kullanıcıyla alındı — yeniden açılmaz)

İstek sisteminden taşınan ilkeler: motor sohbet/varlık/lonca **tanımlamaz**
— tek genel mekanizma; kenarlarda tipli, bayt yalnız telde ve FFI'da;
çekirdek ECS'siz (ECS yapıştırıcısı yalnız `gsb-bevy-ecs`/`gsb-hecs`'te);
her kayıp kendi sayacında; kanal aktörleri, döngü başına tek `await`,
tick asla beklemez; determinizm (olaylar kayıtlı girdilerdir).

| # | Karar | Bu dokümanda |
|---|---|---|
| 1 | **Hedef, olay tipi başına `.proto`'da bildirilir:** `to: WORLD \| CLIENT \| BOTH`. `gsb-build` `impl Event` (op, hedef) üretir; derleme zamanı denetimleri: isteklerle ve olaylarla op çakışması, `reserved`, motor aralığı. | §4.1, §4.2 |
| 2 | **Tick:** CONTROL → RESPONSES → EVENTS → READ → CONVERT → SYSTEMS → FLUSH → BROADCAST. Odanın **kendi** sınırlı olay kanalı (istemci girdi kanalı DEĞİL). Oda API'si `Calls`'ın ikizi: `events.take::<E>()` → sahipli liste (owner, body, from, seq), `take_all()` kanonik sıra; tick sonunda alınmayan sayılır (atılır mı tutulur mu — karar §4.4: atılır). | §3, §4.3, §4.4 |
| 3 | **Yönlendirme:** motor yalnız bir kimliğe, bir odaya ya da herkese yönlendirir. Konu/kanal/lonca/parti üyeliği oyun servisinin işi (`push_many` ile yayar); pub/sub sonra gelebilir. Kimlik → güncel konum registry üzerinden (oda ya da shard aktörü); göç/devam/park yönlendirmeyi doğru tutmalı; çevrimdışı / odasız / parkta → sayılır. CLIENT olayları bağlantının kendi çıkışına (bağlantı aktörü) gider, odadan geçmez. | §6, §7 |
| 4 | **Teslim:** en-fazla-bir-kez + her kayıp sayılır; çevrimdışı gelen kutusu oyun servisinin işi; (yayıncı, kimlik) başına sıra numarası, istemci boşluğu görür. | §6.4, §9, §10 |
| 5 | **Motor kaynaklı olaylar:** servisler motor olaylarına abone olabilir (bağlandı, ayrıldı, odaya girdi, odadan çıktı, devam etti …) — arkadaş servisinin varlık kaynağı. Abonelik API'si servis tarafında (`#[gsb_core::service]` dünyası). | §5.4 |

Yayınlama API'si (her async kod / servis): `events.push(&identity, E)`,
`push_many(&[ids], E)` (yük bir kez kodlanır), `push_room(room, E)`,
`push_all(E)` — bloklamayan `try_send`, tipli retler sayılır. İstemci:
Rust istemcisinde `client.on::<E>()` / `take_events::<E>()`; FFI
`Event::Push` türü; sarmalayıcı eşlemesi (C# event/handler, Godot sinyali,
JS dinleyicisi, Bevy `Event`).

## 3. Tick

```text
global ticker ──TickInfo──▶ oda/shard aktörü (tek await)
   0  CONTROL    join/leave/park/resume/shutdown; shard'da göç kutusu da
                 (Migrate, + bu tur: Forward, Fence — gönderim sırasıyla)
   1  RESPONSES  çağrı sonuçları (değişmedi)
   2  EVENTS     odanın KENDİ olay kutusu (try_recv, kapasite sınırlı):
                 her WORLD bacağı → sahibi çözülür → tipine göre tabloya;
                 çitin arkasında tutulur / göçün hedefine iletilir /
                 sayılır (§7)
   3  READ       üyelerin girdi kanalları (değişmedi)
   4  CONVERT    ingest (değişmedi)
   5  SYSTEMS    oyun: events.take::<E>(), calls.take_*()…
      FLUSH      çağrılar gider (değişmedi)
   6  BROADCAST  tick sonu: alınmayan olaylar atılır + `events_untaken`;
                 alınmayan istekler/sonuçlar (değişmedi); yayın
```

### 3.1 Neden bu sıra

- **RESPONSES'tan sonra:** RESPONSES bu tick'in sonuçlarını tabloya koyar;
  aynı servisin (ör. banka) hem sonucu hem olayı aynı tick'e düşerse
  ikisi de SYSTEMS'te hazırdır, sırayı oyun sistemleri kurar. İkisinin
  arasında taşıma sırası yoktur (§10) — sürüm numarası önerisi.
- **READ'den önce:** bu tick'in girdisi dünyanın en güncel hâline karşı
  doğrulanır. "500 altın geldi" olayıyla 500'lük satın alma aynı tick'e
  düşerse ve oyun olayları önce alıyorsa satın alma geçer (spike
  `paths::gold_arriving_this_tick_pays_for_a_purchase_this_tick`). Bu,
  RESPONSES'u READ'den önce koyan s0 gerekçesinin aynısıdır.
- **Kendi kanalı:** odanın olay kutusu üyelerin girdi kanallarından
  ayrıdır. Girdi kanalını dolduran istemci yalnız kendi girdisini düşürür
  (bağlantı aktörü sayar, değişmedi); olayları başka bir kanaldadır (spike
  `paths::a_spamming_client_does_not_lose_its_events`: 10 tick boyunca tick
  başına 1000 girdi dener, ~9 900'ü düşer; 50 olayın 50'si gelir).

### 3.2 Boşta maliyet

Sessiz bir odada EVENTS = tek boş `try_recv`. Tablonun tamponu
(`Vec::new()`) hiç ayrılmaz; sayaç oynamaz. Abonesiz registry'de motor
olayı yayımı boş bir döngüdür. Spike `idle`: 1000 boş tick sonra tek oda
ve iki shard'ın tablo kapasitesi 0, tüm sayaçlar sıfır.

## 4. Tanım ve tipli kenarlar

### 4.1 `.proto`: `(gsb.event)`

`gsb/options.proto`'ya eklenenler (spike'ın kopyası protoc'tan geçer —
`spikes/e0-events/proto/gsb/options.proto`):

```proto
enum Destination {
  DESTINATION_UNSPECIFIED = 0;  // gsb-build reddeder
  WORLD = 1;
  CLIENT = 2;
  BOTH = 3;
}

message EventOptions {
  uint32 op = 1;
  Destination to = 2;
}

extend google.protobuf.MessageOptions {
  EventOptions event = 50004;      // (gsb.event)
}

extend google.protobuf.FileOptions {
  repeated uint32 reserved_ops = 50005;   // (gsb.reserved_ops): servissiz dosyanın emekli op'ları
}
```

Oyunun yazdığı (spike `tests/proto/game.proto`):

```proto
import "gsb/options.proto";

option (gsb.reserved_ops) = 2099;   // emekli bir olay op'u

message GoldReceived   { option (gsb.event) = { op: 2001, to: WORLD  }; string from = 1; uint64 amount = 2; uint64 balance = 3; uint64 version = 4; }
message ChatMessage    { option (gsb.event) = { op: 2002, to: CLIENT }; string channel = 1; string from = 2; string text = 3; }
message FriendOnline   { option (gsb.event) = { op: 2003, to: CLIENT }; string who = 1; }
message FriendOffline  { option (gsb.event) = { op: 2004, to: CLIENT }; string who = 1; }
message BalanceChanged { option (gsb.event) = { op: 2005, to: BOTH   }; uint64 balance = 1; uint64 version = 2; string reason = 3; }
```

Biçim neden bu: seçenek **mesajın kendisinde** durur (olay bir mesajdır;
`rpc` gibi bir sarmalayıcısı yok); tek bir ileti-tipli seçenek, iki ayrı
skaler seçenekten (`(gsb.event_op)`, `(gsb.event_to)`) iyidir — biri
unutulunca "yarım olay" doğmaz, protoc'un toplu sözdizimi (`{ op: …, to:
… }`) tek satırdır. `to:` değerleri `gsb` paketinin kapsamındadır (proto
enum değerleri paket kapsamlıdır); oyun paketleriyle çakışmaz.

**Bir op uzayı.** İstek op'ları (`(gsb.op)`) ve olay op'ları aynı oyun
bandını (`≥ 1000`) paylaşır: kayıt, günlük ve genel bir dağıtıcı bir op'u
hangi zarfın taşıdığını söylemeden adlandırabilir; tek çakışma denetimi.
`(gsb.reserved_op)` (servis seçeneği, q3) iki uzayı da kapsar;
`(gsb.reserved_ops)` servis bildirmeyen dosyalar için.

**`gsb-build` reddeder** (hepsi listelenir — q3'ün `Problem` deseni; spike
`codegen::the_checks_refuse_every_bad_declaration`): motor aralığında op
(`< 1000`), 16 biti aşan op, `to:`'suz (ya da `UNSPECIFIED`) olay,
`op`'suz olay, istekle ya da başka olayla aynı op, ayrılmış op, hem bir
rpc'nin isteği hem olay olan mesaj (tipin iki `OP`'u olurdu).

**Üretilen** (spike `codegen::generate`'in çıktısı `src/game.rs`'teki
satırların aynısı; test bunu denetler):

```rust
impl Event for GoldReceived { const OP: u16 = 2001; const TO: Dest = Dest::World; }
impl WorldEvent for GoldReceived {}
impl Event for ChatMessage { const OP: u16 = 2002; const TO: Dest = Dest::Client; }
impl ClientEvent for ChatMessage {}
impl Event for BalanceChanged { const OP: u16 = 2005; const TO: Dest = Dest::Both; }
impl WorldEvent for BalanceChanged {}
impl ClientEvent for BalanceChanged {}
```

### 4.2 Sözleşme

`gsb_protocol::event` (istek sözleşmesi gibi protokol crate'inde: tipli
istemci de kullanır; `gsb_core::events` yeniden dışa açar):

```rust
pub enum Dest { World, Client, Both }
pub trait Event: WireMessage + Clone + Debug { const OP: u16; const TO: Dest; }
pub trait WorldEvent: Event {}    // to: WORLD | BOTH  — oda alabilir
pub trait ClientEvent: Event {}   // to: CLIENT | BOTH — istemci dinleyebilir
```

Hedef tipin **sabiti** ve iki işaretçi trait'tir; yanlış taraf derleme
hatasıdır, sessiz boş liste değil (spike `src/lib.rs` doc testleri, elle
de derlendi): odada `events.take::<ChatMessage>()` → E0277
(`ChatMessage: WorldEvent` değil); istemcide `take::<GoldReceived>()` →
E0277; `publisher.push(&id, &TransferGold{..})` (istek tipi) → E0277;
boot'ta `events.accept::<FriendOnline>()` → E0277; yanlış gövde tipli
dinleyici `c.on::<ChatMessage>(|e: Pushed<FriendOnline>| ..)` → E0631.

### 4.3 Oda API'si: `Events` tablosu (`Calls`'ın ikizi)

| Yöntem | Ne yapar |
|---|---|
| `events.accept::<E: WorldEvent>()` | Boot'ta: bu oda `E`'yi alır (op → kod çözücü). Kabul edilmemiş op EVENTS'te `events_no_handler` sayılır, oyuna ulaşmaz. Aynı op iki kez → `BootError::EventOpCollision`. |
| `events.take::<E: WorldEvent>() -> Vec<Arrived<E>>` | Bu tick'in `E` olayları, varış sırasıyla. |
| `events.take_all() -> Vec<AnyArrived>` | Bu tick'in tüm olayları, kanonik (EVENTS boşaltma) sırasıyla; `a.downcast::<E>()`. |

```rust
pub struct Arrived<E> {
    pub body: E,
    pub to: Addressee,     // Player(Identity) | Room
    pub owner: Owner,      // kimliğin bağlı olduğu sahip (oda çapında: Owner::ACTOR)
    pub from: PublisherId, // yayıncı (0 = motor)
    pub seq: u64,          // WORLD bacağının sıra numarası (yayıncı → bu alıcı)
}
```

`owner`, `Calls`'ın aynı opak sahibidir: oturumun açık bağı
(`calls.bind`), yoksa `GameLogic::call_owner(player)`. bevy/hecs'te
`e.owner.entity()`. Sahip EVENTS'te çözülür: motor odada kimlik → oyuncu
eşlemini tutar (yeni küçük bir indeks; bugün oda `ConnId → PlayerId`
tutuyor, kimlik join'de zaten geliyor).

**Motor kancası (eklemeli):** `GameLogic::boot_events(&mut Events) ->
Result<(), BootError>` ve `GameLogic::update_with_events(world, ctx, &mut
Calls, &mut Events)` — varsayılanı `update_with_calls(world, ctx, calls)`
(olay almayan mevcut oyunlar değişmeden derlenir; aldıkları olmadığı için
hiçbir şey sayılmaz). Shard'da `ShardLogic::update_seam_with_events`.
(Tek bir `Io { calls, events }` demeti: §20 E6.)

### 4.4 Alınmayan olay: atılır ve sayılır (KARAR)

Tick sonunda tabloda kalan olay **atılır**, `events_untaken` sayılır;
bir sonraki tick'e taşınmaz. Gerekçe:

1. **Gizli kuyruk yok.** Taşınan olay, hiç almayan bir oyunda sınırsız
   birikir (ya da yeni bir sınır + yeni bir kayıp sayacı ister).
2. **Hata görünür kalır.** Olayı almayı unutan sistem (yanlış tip,
   koşullu `take`) bir tick sonra "geç" çalışıyormuş gibi görünmez;
   sayaç hemen söyler. İstek tarafının `requests_untaken`'ı ile aynı ilke.
3. **Kayıt basit kalır.** Taşıma, kaydın tick'ler arası durum
   taşımasını gerektirirdi; atmak her tick'in girdisini kendi içinde tam
   tutar.
4. Olayı bilinçli ertelemek isteyen oyun onu kendi durumuna kopyalar
   (bir bileşen, bir kuyruk) — oyunun kararı, oyunun sınırı.

### 4.5 ECS biçimleri

Çekirdek ECS'siz; adaptörler `Calls`'takiyle aynı deseni izler:

```rust
// ECS'siz: &mut Events geçilir; owner oyunun kendi id'si
for e in events.take::<GoldReceived>() {
    if let Some(p) = self.players.get_mut(&e.owner.0) { p.gold_seen(e.body.balance, e.body.version) }
}

// bevy_ecs (gsb-bevy-ecs::events): GsbEvents(Events) kaynağı, Calls gibi ödünç
fn update_with_events(&mut self, world: &mut World, _ctx: &TickCtx, calls: &mut Calls, events: &mut Events) {
    gsb_bevy_ecs::events::lend(world, events, |w| gsb_bevy_ecs::lend(w, calls, |w| self.schedule.run(w)));
}
fn gold(mut events: ResMut<GsbEvents>, mut q: Query<&mut Wallet>) {
    for e in events.take::<GoldReceived>() {
        if let Some(Ok(mut w)) = e.owner.entity().map(|x| q.get_mut(x)) { w.apply(e.body.balance, e.body.version) }
    }
}

// hecs (gsb-hecs): düz fonksiyon
fn gold(w: &mut hecs::World, events: &mut Events) {
    for e in events.take::<GoldReceived>() {
        let Some(x) = e.owner.entity() else { continue };
        if let Ok(wallet) = w.query_one_mut::<&mut Wallet>(x) { wallet.apply(e.body.balance, e.body.version) }
    }
}
```

`gsb_bevy_ecs::events::lend` iki `mem::swap`'tır (q5'in `lend`'i gibi;
ilk çağrıda kaynak bir yedekle eklenir, sonra tahsis yok).

## 5. Yayınlamak

### 5.1 `Publisher`

```rust
#[derive(Clone)]
pub struct Publisher { /* id + yönlendiricinin kutusu + atomik sayaçlar */ }

impl Publisher {
    pub fn push<E: Event>(&self, to: &Identity, e: &E) -> Result<(), PushRefused>;
    pub fn push_many<E: Event>(&self, to: &[Identity], e: &E) -> Result<(), PushRefused>;
    pub fn push_room<E: Event>(&self, room: RoomId, e: &E) -> Result<(), PushRefused>;
    pub fn push_all<E: Event>(&self, e: &E) -> Result<(), PushRefused>;
    pub fn id(&self) -> PublisherId;
}
pub enum PushRefused { RouterFull, RouterGone }
```

- **Tek `try_send`.** Her itme yönlendiricinin sınırlı kutusuna bir
  mesajdır; asla beklemez — bir sistem tick içinden, bir servis çağrının
  içinden iter. Kutu dolu → `Err(RouterFull)` + `events_refused_router_full`;
  yönlendirici yok (sunucu duruyor) → `Err(RouterGone)`. Bunlar **anlık,
  tipli** retlerdir; sonrası (çevrimdışı, dolu oda, eski istemci …)
  yönlendiricide ve aşağıdaki aktörlerde adıyla sayılır, yayıncıya dönmez
  (en-fazla-bir-kez; geri bildirim kanalı ikinci bir yol olurdu).
- **Bir kez kodlama.** Yük tutamaçta bir kez `Bytes`'a kodlanır; `push_many`
  10 000 kimliğe de tek kodlama ve tek mesajdır (`Arc<[Identity]>`).
  Yönlendirici `Bytes`'ı referans sayımıyla paylaştırır; alıcı başına iş
  (küçük başlık + gövdenin kopyası) alıcıların bağlantı aktörlerindedir.
  Spike `paths::push_many_encodes_once_and_shares_the_body`: 1000 alıcı,
  `encodes == 1`, yönlendiriciye tek mesaj, 1000 bağlantı aktörü aynı
  gövde adresini gördü.
- **Kim alır:** kompozisyon kökü üretir — `let bank_events =
  parts.publisher("bank");` (ad metrik etiketi ve günlük için; id
  `PublisherId`, 0 motora ayrılmış). Servis implementasyonuna yapıcısında
  verilir; odaya `Svc<Publisher>` (bevy) ya da düz alan olarak. REPLAY'de
  odanın tutamacı **bağlantısız** bir tutamaçtır (itmeler hiçbir yere
  gitmez, sayılmaz — çıktı, girdi değil; `ReplayHost` istemcileri gibi).

### 5.2 Hedef

```rust
pub enum Target { One(Identity), Many(Arc<[Identity]>), Room(RoomId), All }
```

Motor yalnız bunları bilir (karar 3). Kanal/konu/lonca/parti üyeliği
servistedir: sohbet servisi kanalın üye listesini tutar ve `push_many`
ile yayar (§8.2). `push_room`: CLIENT bacağı odanın o anki üyelerine
(registry'nin oda → kimlik indeksi), WORLD bacağı odanın mantığına (her
shard'a bir kez; `to = Room`, `owner = ACTOR`). `push_all`: her oturum /
her oda.

### 5.3 İki bacak

`BOTH` olay iki bağımsız bacaktır: CLIENT bacağı bağlantıya, WORLD bacağı
odaya. Her bacak kendi sırası, kendi sayacı, kendi kaybıdır; aralarında
sıra yoktur (§10).

### 5.4 Motor kaynaklı olaylar ve abonelik

Registry oturum ve oda yaşam döngüsünün her olgusunu zaten öğrenir
(AUTH, SpawnDone, LeaveDone, Detached, devam, ConnClosed). Bunları
**servislere** sunar (istemcilere değil; odalar bunları CONTROL'den zaten
öğrenir):

```rust
#[non_exhaustive]
pub enum EngineEvent {
    Online  { identity: Identity },                     // kimliğin ilk doğrulanmış oturumu açıldı
    Offline { identity: Identity },                     // oturum bitti ve hiçbir şey tutmuyor (park değil)
    Joined  { identity: Identity, room: RoomId },
    Left    { identity: Identity, room: RoomId },       // ayrıldı ya da odada çevrimdışı oldu
    Parked  { identity: Identity, room: Option<RoomId> },  // taşıma öldü, varlık parkta
    Resumed { identity: Identity, room: Option<RoomId> },  // parktaki kimlik yeni oturumla döndü
}
pub struct Interest(u32);   // ONLINE | OFFLINE | JOINED | LEFT | PARKED | RESUMED; PRESENCE; ALL
```

Anlamlar: aynı kimliğin ikinci oturumu (eskisini geçersiz kılan)
`Online` üretmez — kimlik zaten çevrimiçi; `Parked` `Offline` değildir
("uzakta" mı "çevrimdışı" mı servisin yorumu); anonim (boş kimlikli)
oturum için olay üretilmez ve yönlendirilemez. Bir kimliğin olayları
registry'nin işleme sırasıyla gelir (`Online`, `Joined`'dan önce).

**Servis tarafı API'si** — iki biçim:

```rust
// (a) #[gsb_core::service] trait'inde işaretli tek metot (makro):
#[gsb_core::service]
pub trait Friends {
    async fn list(&self, cx: &CallCx) -> Result<FriendList, GameError>;      // istemci isteği (forward)
    #[gsb(engine_events)]
    async fn on_engine(&self, e: EngineEvent);                              // cx yok, Result yok
}
parts.service(Friends::serve(PgFriends::new(db, parts.publisher("friends"))),
              |s| s.engine_events(Interest::PRESENCE))?;

// (b) herhangi bir async görev: düz sınırlı alıcı
let mut rx = parts.engine_events("presence-audit", Interest::ONLINE.or(Interest::OFFLINE), 1024);
tokio::spawn(async move { while let Some(e) = rx.recv().await { audit(e).await } });
```

(a)'da makro metodu posta kutusu enum'una bir kol olarak ekler
(`friends::Msg::Engine(EngineEvent)`); host onu tower yığınından geçirmeden
(cevap hakkı yok — ateşle ve unut) servisin kendi görevinde koşturur;
registry abonenin posta kutusuna `try_send` eder. Metot imzası denetlenir
(`&self` + tek `EngineEvent`, dönüş `()`; trait başına en çok bir tane).
(b) aynı kayıt, makrosuz. Dolu/kapanmış abone **abone adıyla** sayılır:
`engine_events_sent{subscriber}`, `engine_events_refused_full{subscriber}`,
`engine_events_refused_gone{subscriber}` (spike
`presence::a_full_subscriber_is_counted_per_name`). Motor varlık geçmişi
tutmaz: kaçıran servis doğruyu yeniden okur (yeniden başlatmada
"şu an kim çevrimiçi" için `Handle` ile bir registry sorgusu — §20 E12).

Motor olayları kaydedilmez: servisler determinizm sınırının dışındadır
(SERVICES §10.4); odaya etkileri yalnız olay ya da çağrı sonucu olarak
girer ve o kayıttadır.

## 6. Yönlendirme

### 6.1 Yönlendirici: registry'nin kimlik tablosu

```text
Publisher ──try_send──▶ [yönlendirici = registry: kimlik → satır]
                         │ satır yok → events_offline
                         │ her alıcı, her bacak: sıra numarası damgala
                         │
          CLIENT bacağı  │                         │  WORLD bacağı
                         ▼                         ▼
     oturum yok (park) → client_parked     konum yok → world_no_room
     FEATURE_PUSH yok → client_unsupported odanın/shard'ın OLAY KUTUSU (try_send)
     bağlantı aktörü (ConnIn::Push)          dolu → world_refused_room_full
       dolu/kapalı → client_refused_*        yok  → world_refused_room_gone
                         │                         │
                         ▼                         ▼
     PUSH karesi bağlantının çıkışına      EVENTS fazı → Events tablosu
     (oda yolda yok)                       (§3; göç: §7.3)
```

Satır (kimlik başına):

```rust
struct Row {
    session: Option<ConnLink>,  // bağlantı aktörünün kutusu + Auth.features; None = parkta
    parked: bool,
    place: Place,               // Nowhere | In(RoomId, shard)
    mig: u64,                   // bildirilen en yüksek göç sayısı (§7.3)
    seqs: Vec<(PublisherId, Leg, u64)>,  // bir avuç giriş
}
```

Registry bugün `ConnectionId → ConnInfo` tutuyor; e2 `Identity → Row`
indeksini ekler (yalnız kimlikli oturumlar), `ConnInfo` `features` alır.
Olguları zaten öğrendiği yerlerde günceller: `Authed` (+ `features`,
eklemeli alan) → `Online`; `SpawnDone` (+ kabul eden shard indeksi) →
`Joined`; `LeaveDone` → `Left`; `Detached` → `Parked`; devam (yeniden
bağlama) → `Resumed`; `ConnClosed` (park değil) → `Offline`; yeni tek
mesaj: shard'dan `Relocated` (§7.3). Oda yaratma/yok etme olay kutularını
kaydeder/siler (`CreateRoom`, `RoomDied`, yeniden kurulum yeni kutular).

**Neden registry (karar 3) ve ne zaman bölünür:** kimlik → oturum ve oturum
→ oda bilgisi zaten orada, her olguyla güncel; ikinci bir kopya
tutarlılık işi olurdu. Risk: itme trafiği (sohbet) kontrol düzleminin
kutusuna biner — join/leave gecikmesi. **Ölç, sonra böl** (§12): tablo düz
bir struct'tır (`EventRoutes`), registry aktörünün içinde koşar; bölmek
gerekirse aynı struct ayrı bir `EventRouter` aktörüne (ya da kimlik
hash'iyle K tanesine) taşınır ve registry ona olguları iletir —
yönlendirme kodu değişmez (spike'ın yönlendiricisi zaten ayrı bir aktör
olarak modellendi).

### 6.2 CLIENT bacağı: bağlantı aktörü

Yönlendirici bacağı bağlantı aktörünün kutusuna koyar (`ConnIn::Push {
publisher, seq, op, body }`, yeni kol); aktör onu `PUSH` karesine (§14)
çevirir ve bağlantının **kendi çıkışına** yazar — odanın yayınının ve
aktörün kontrol cevaplarının paylaştığı aynı sınırlı kanal. Oda yolda
yoktur: sohbet hiçbir odaya bir şeye mal olmaz, odası olmayan (lobi
öncesi) oyuncuya da ulaşır (spike `paths::a_client_event_never_touches_a_room`:
oda tablosu ve kaydı boş, yönlendiricide `world_sent = 0`). Çıkış dolu
(yavaş istemci) → `pushes_dropped_outbound_full`; yazar yok →
`pushes_dropped_outbound_gone` (bağlantının `ConnSample`'ında).

### 6.3 WORLD bacağı: odanın kendi kutusu

Her oda (ve her shard) bir sınırlı olay kutusu alır
(`RoomConfig::event_capacity`, varsayılan önerisi 1024 — §12).
Yönlendirici `try_send` eder; EVENTS onu boşaltır. Kutu **odanındır**,
oturumun değil: istemcinin girdisinden yalıtılmıştır (§3.1) ve oturum
olmadan da (parktaki varlık) çalışır.

### 6.4 Sıra numaraları

Yönlendirici her bacağı, her alıcı için damgalar: **(yayıncı, kimlik,
bacak) başına 1'den artan** numara. Bacak ayrımı şarttır (spike bulgusu):
tek numara olsaydı istemci, aynı yayıncının WORLD-yalnız olaylarını
boşluk sanırdı (`paths::both_runs_both_legs_with_their_own_numbers`).
Numara bacağın **yönlendirildiği** anda tüketilir; bacak aşağıda
kaybolursa (park, eski istemci, dolu kutu) alıcı bir sonrakinde boşluğu
görür. Numaralar satır yaşadıkça sürer: ilk doğrulanmış oturumdan,
kimlik çevrimdışı (ve park değil) olana kadar — devam numarayı korur, yeni
oturum (satır yeniden doğar) 1'den başlar (§20 E5: akış kimliği). Oda
çapındaki WORLD olayları (yayıncı, oda) başına numaralanır.

## 7. Konum değişiklikleri

### 7.1 Oda değiştirme (ayrıl → katıl)

Ayrılış `Left` ile konumu `Nowhere` yapar; katılış `Joined` ile yeni
odayı. Arada gelen WORLD bacağı `world_no_room` sayılır; eski odanın
kutusunda bekleyen bacak eski odanın EVENTS'inde `events_world_not_here`
sayılır (CONTROL ayrılışı EVENTS'ten önce işlediği için — spike
`routing::leaving_between_route_and_drain_is_counted_not_here`). Kayıp
**sayılır, yeniden sıralanmaz**. Yeni oda oyuncunun durumunu zaten yükler
(SERVICES §6.2); servisin otoritesi (bakiye veritabanında) değişmez.

### 7.2 Park ve devam

Park: oturum `None`, satır ve konum kalır. CLIENT bacağı
`client_parked` sayılır (numara tüketilir). WORLD bacağı **teslim
edilir**: varlık odadadır, sahibi yaşıyor (§20 E1 — s0'ın "parkta WORLD
düşer" duruşunun tersi; öneri bu). Devam: `Resumed` yeni oturumu satıra
koyar; numaralar sürer, istemci kaçırdığını boşluk olarak görür ve yeniden
eşitler (spike `routing::park_and_resume_keep_routing_and_the_client_sees_its_gap`:
sohbet 1 geldi, 2 parkta sayıldı, 3 geldi → istemci `Gap { missed: 1 }`;
parktayken gelen altın odadaki cüzdana yazıldı). Devam aynı odaya/shard'a
olur (park defteri oradadır), WORLD konumu değişmez.

### 7.3 Shard göçü: mezar taşı, ileri gönderme, çit

Sorun (spike'ta sahnelendi): bob shard 0'dan 1'e geçerken yönlendirici
henüz bilmiyor; 1–3 numaralı olaylar shard 0'ın kutusunda. Shard 1 göçü
alıp `Relocated` bildirir, yönlendirici 4–6'yı **doğrudan** shard 1'e
gönderir; shard 1, shard 0 ileri göndermeden önce tick'lerse oda
**4, 5, 6, 1, 2, 3** görür (`routing::without_the_fence_a_migration_reorders`).
Yayıncı → kimlik FIFO'su göçte kırılır. Çözüm, registry'de tampon
olmadan:

```text
kaynak shard A                  yönlendirici                    hedef shard B
  göç (seam): MigrateIn ─────────── shard FIFO ─────────────▶  CONTROL: üyeyi kur,
  mezar taşı: bob → B                                           TUT(bob, mig) aç,
                                                               ◀─ Relocated{bob,B,mig}
                                 konum := B; mig := mig
                                 SEAL{bob,mig} ──▶ A'nın OLAY kutusu
                                 (A'ya önceden gidenlerin ARKASINDA)
                                 bundan sonrası doğrudan B'ye ──▶ EVENTS: bob TUT'ta → beklet
  EVENTS: bob'a gelenler → mezar
  taşı → Forward ───────────────── shard FIFO ─────────────▶  CONTROL: Forward → tabloya
  SEAL → FENCE{bob,mig} ────────── shard FIFO ─────────────▶  CONTROL: FENCE → bekletilenleri
                                                               sırayla bırak; TUT kapanır
```

- **Seal**, A'nın olay kutusunda A'ya gönderilmiş her şeyin arkasındadır
  (tek kanal FIFO); **Fence**, shard FIFO'sunda A'nın ileri gönderdiği
  her şeyin arkasındadır. B, doğrudan gelenleri çite kadar beklettiği için
  sıra korunur (`routing::a_migration_keeps_per_publisher_fifo_with_the_fence`:
  1…6). Bedel: göçten sonra doğrudan bacaklar bir seal tur süresi (≈1–2
  tick) bekler.
- **Zincirli göç** (A → B → C, A'nın çiti B'ye varmadan): bir kural daha.
  Gelen çitini hâlâ bekleyen bir shard, oyuncu ondan da ayrılınca **kendi**
  doğrudan bacaklarını (tuttukları + sonradan gelenler) bekletir — onlar,
  önceki kaynağın hâlâ ileri göndereceklerinden yenidir; ileri gönderilmiş
  bacakları ise hemen aktarır. Giden çiti ancak gelen çit vardıktan
  sonra yollar. Spike `routing::a_chained_migration_keeps_fifo_too`:
  her turda en kötü sırayla (önce C, sonra B, sonra A) tick'lenen üç
  shard, 1…5, zaman aşımı yok.
- **`mig`** oyuncunun göç sayacıdır (`PlayerMigration`'a eklemeli alan):
  geç gelen eski bir `Relocated` kazanamaz (`relocations_stale`).
- **Sınırlar ve kayıplar:** mezar taşı `TOMB_TTL` tick ileri gönderir,
  sonra `events_tombstones_expired` ve gelenler `events_world_not_here`;
  çit `FENCE_TTL` tick içinde gelmezse bekletilenler yine de bırakılır
  (`events_fence_timeouts` — sıra garantisi o olayda düşer, sayılır);
  bekleme kuyruğu `HOLD_CAP`'te taşarsa `events_fence_overflow`; kimsenin
  beklemediği seal/çit `events_fence_lost`; seal gönderilemezse
  `events_seals_refused` (hedef zaman aşımıyla bırakır); `Relocated`
  gönderilemezse hedef tutmaz, kaynak mezar taşı süresince ileri gönderir
  (`events_relocations_refused`). Spike değerleri: 64 / 64 / 256 (§20 E8).
- **İleri gönderme neden shard FIFO'sundan** (registry'ye geri değil):
  `Migrate` zaten o kanaldadır; ileri gönderilen olay onun arkasına girer
  ve hedef onu oyuncu kurulduktan sonra görür — ertelenmiş göç (kurulum
  kapısı açılmamış) bile aynı ertelemeyle sırasını korur. Registry'ye
  geri göndermek ikinci bir tur ve kontrol düzlemine ek yük olurdu (§19 E-A12).

### 7.4 Oda ölümü ve yeniden kurulum

Ölen odanın kutusu kapanır: yönlendirici `world_refused_room_gone` sayar.
Registry `RoomDied`'da oda girdisini ve üyelerin konumunu temizler (bugünkü
`notify_room_gone` ile aynı yerde); yeniden kurulumda yeni kutular
kaydedilir. Kutuda kalan olaylar ölümle gider, sayılmaz (panikle ölen
görev sayacını da götürür — bugünkü B67 duruşu).

## 8. Örnekler

Ortak: olaylar §4.1'deki `game.proto`'dan; oturumlar SERVICES §5.6'daki
gibi bağlı (`owner` hazır). Spike'taki karşılıkları `harness::Wallets`
(ECS'siz oda) ve testler.

### 8.1 Altın gönderme → `GoldReceived` (WORLD) — bob başka odada / shard'da

Alice'in isteği SERVICES §6.1'deki gibi `.answers` ile bankaya gider.
Banka, tek etkili aktarımı yapar, **alice'e cevabı döner ve bob'a
olayı iter**:

```rust
struct PgBank { db: PgPool, events: Publisher }

impl Bank for PgBank {
    async fn transfer(&self, cx: &CallCx, to: Identity, amount: u64) -> Result<TransferDone, GameError> {
        let from = cx.player().ok_or(err(NOT_LOADED))?;
        let t = self.db.transfer_once(cx.key(), from, &to, amount).await?;   // aynı anahtar = aynı sonuç
        if t.first_time {                                                     // yeniden yayında ikinci itme yok
            let _ = self.events.push(&to, &GoldReceived {
                from: from.as_str().into(), amount,
                balance: t.to_balance, version: t.to_version,                 // defterin sürümü
            });
        }
        Ok(TransferDone { balance: t.from_balance })
    }
}
```

(`let _ =`: itmenin reti sayılmıştır; banka için olay bir bildirim,
otorite veritabanında. Göçte aynı anahtarla yeniden yayın (SERVICES §8)
ikinci kez itmesin diye "ilk kez mi" defterden gelir.)

**Bob'un odası** — ECS'siz (spike `Wallets` aynı mantık):

```rust
fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> { events.accept::<GoldReceived>() }

fn update_with_events(&mut self, w: &mut MyWorld, ctx: &TickCtx, calls: &mut Calls, events: &mut Events) {
    for e in events.take::<GoldReceived>() {                 // önce olaylar …
        if let Some(p) = w.players.get_mut(&e.owner.0) {
            if e.body.version > p.wallet.version {            // sürüm: cevap/olay sırası yok (§10)
                p.wallet.gold = e.body.balance;
                p.wallet.version = e.body.version;
                p.toasts.push(format!("{} sent you {}", e.body.from, e.body.amount));
            }
        }
    }
    self.transfers(w, calls);                                 // … sonra istekler (§3.1)
}
```

bevy_ecs ve hecs biçimleri §4.5'teki `gold` sistemleridir (aynı sürüm
kuralı). **Shard'da** kod aynıdır: bob hangi shard'daysa olay oraya gider;
geçiş sırasında §7.3 (spike `routing`'in üç göç testi bu olayla).

**İstemci:** `GoldReceived` WORLD-yalnızdır — istemci onu dinleyemez
(derleme hatası); bob yeni bakiyeyi odanın snapshot'ında / özel
karesinde görür (oyunun kararı). Bildirim de istenirse olay `BOTH`
bildirilir (§8.4'teki gibi).

### 8.2 Sohbet → `ChatMessage` (CLIENT) — bir kanalın üyelerine

İstek `forward` ile doğrudan sohbet servisine gider (SERVICES §5.8; oda
görmez); servis kanalı (oyunun kavramı) tutar ve yayar:

```rust
calls.forward(move |r: SendChat| chat.send(r))?;            // her odanın boot'unda, aynı satır

struct ChatSvc { channels: Channels /* kanal → üyeler (servisin durumu) */, events: Publisher }

impl Chat for ChatSvc {
    async fn send(&self, cx: &CallCx, r: SendChat) -> Result<Sent, GameError> {
        let me = cx.player().ok_or(err(NOT_LOADED))?;        // doğrulanmış kimlik, yükten değil
        let members = self.channels.members_if_member(&r.channel, me).await.ok_or(err(NOT_IN_CHANNEL))?;
        self.rate.check(me)?;                                 // susturma/hız sınırı: oyunun işi
        let line = ChatMessage { channel: r.channel, from: me.as_str().into(), text: r.text };
        self.events.push_many(&members, &line).map_err(|_| err(BUSY))?;   // tek kodlama
        Ok(Sent {})
    }
}
```

Oda tarafı: **yok** (CLIENT). İstemci tarafı:

```rust
// Rust istemcisi (gsb_client::events)
client.events().on::<ChatMessage>(|e| ui.chat.push(e.body.channel, e.body.from, e.body.text));
// ya da kare döngüsünde: for e in client.events().take::<ChatMessage>() { … }
for gap in client.events().take_gaps() { ui.chat.mark_gap(gap.from, gap.missed) }   // "birkaç mesaj kaçırıldı"
```

```csharp
// C# (Unity) — .proto'dan üretilmiş olay yüzeyi
game.Events.ChatMessage += e => chatView.Append(e.Channel, e.From, e.Text);
game.Events.Gap += (publisher, missed) => chatView.MarkGap(missed);
```

Sohbetin geçmişi, çevrimdışı teslim, moderasyon: servisin (karar 4).

### 8.3 Arkadaş varlığı: motor olayları → arkadaş servisi → `FriendOnline` (CLIENT)

```rust
impl Friends for PgFriends {
    async fn list(&self, cx: &CallCx) -> Result<FriendList, GameError> { /* istek: forward */ }

    async fn on_engine(&self, e: EngineEvent) {               // #[gsb(engine_events)]
        match e {
            EngineEvent::Online { identity } | EngineEvent::Resumed { identity, .. } => {
                let online = self.presence.mark_online(&identity);           // servisin durumu
                let friends = self.graph.online_friends(&identity, &online).await;
                let _ = self.events.push_many(&friends, &FriendOnline { who: identity.as_str().into() });
                for f in &friends { let _ = self.events.push(&identity, &FriendOnline { who: f.as_str().into() }); }
            }
            EngineEvent::Offline { identity } | EngineEvent::Parked { identity, .. } => {
                if self.presence.mark_offline(&identity) {
                    let friends = self.graph.online_friends(&identity, &self.presence.snapshot()).await;
                    let _ = self.events.push_many(&friends, &FriendOffline { who: identity.as_str().into() });
                }
            }
            _ => {}
        }
    }
}
// kompozisyon kökü
parts.service(Friends::serve(PgFriends::new(db, parts.publisher("friends"))), |s| s.engine_events(Interest::PRESENCE))?;
```

Spike `presence::presence_flows_from_the_engine_through_a_service_to_friends`
bunu canlı koşar (yönlendirici ve servis tokio görevleri, her biri tek
`await`): bob ve carol çevrimiçi, sonra alice → bob ve carol
`FriendOnline(alice)`, alice `FriendOnline(bob)` + `FriendOnline(carol)`;
alice çevrimdışı → bob `FriendOffline(alice)`.

İstemci:

```rust
client.events().on::<FriendOnline>(|e| ui.friends.set_online(&e.body.who, true));
client.events().on::<FriendOffline>(|e| ui.friends.set_online(&e.body.who, false));
// boşluk görülürse liste yeniden istenir:
if !client.events().take_gaps().is_empty() { client.send::<ListFriends>(&ListFriends {}); }
```

```csharp
game.Events.FriendOnline  += e => friendsView.SetOnline(e.Who, true);
game.Events.FriendOffline += e => friendsView.SetOnline(e.Who, false);
game.Events.Gap += (_, _) => _ = game.ListFriends(new ListFriends());   // yeniden eşitle
```

### 8.4 Web mağazası satın alımı → `BalanceChanged` (BOTH)

Satın alım odanın dışında olur (HTTP arka ucu → mağaza servisi; admin
yolu, `Handle` ile):

```rust
// web arka ucunun işleyicisi (oda dışı, oyuncu adına)
let done = shop_handle.on_behalf_of(&buyer).purchase(sku).await?;   // tek etki, defterde
// mağaza servisinin içinde, ödeme işlendikten sonra:
let _ = self.events.push(&buyer, &BalanceChanged { balance: t.balance, version: t.version, reason: "web shop".into() });
```

**Oda** (WORLD bacağı) — ECS'siz:

```rust
for e in events.take::<BalanceChanged>() {
    match e.to {
        Addressee::Player(_) => if let Some(p) = w.players.get_mut(&e.owner.0) {
            if e.body.version > p.wallet.version { p.wallet.gold = e.body.balance; p.wallet.version = e.body.version; }
        },
        Addressee::Room => w.banner = Some(e.body.reason.clone()),   // push_room ile oda çapında
    }
}
```

bevy_ecs: `fn balance(mut events: ResMut<GsbEvents>, mut q: Query<&mut Wallet>)` aynı
gövde, `e.owner.entity().map(|x| q.get_mut(x))`; hecs:
`w.query_one_mut::<&mut Wallet>(x)`.

**İstemci** (CLIENT bacağı) — bildirim:

```rust
client.events().on::<BalanceChanged>(|e| ui.toast(format!("+{} ({})", e.body.balance, e.body.reason)));
```

```csharp
game.Events.BalanceChanged += e => toast.Show($"Balance: {e.Balance} — {e.Reason}");
```

Spike `paths::both_runs_both_legs_with_their_own_numbers`: oda cüzdanı en
yeni sürümle günceller, istemci iki bildirimi 1, 2 numarasıyla alır; aynı
yayıncının WORLD-yalnız `GoldReceived`'ı istemcide boşluk değildir.

### 8.5 Sarmalayıcı eşlemesi

| Dil / ortam | Dinleme biçimi | Kaynak |
|---|---|---|
| Rust | `client.events().on::<E>(f)` / `take::<E>()` / `take_gaps()` | `gsb_client::events` (sans-IO) |
| C# (Unity) | `game.Events.FriendOnline += handler;` (üretilmiş `event Action<FriendOnline>`), `game.Events.Gap` | C ABI, `Event::Push` / `Event::Gap` |
| Godot (gdext) | sinyal: `friend_online(who)`; `client.connect("friend_online", …)` | Rust çekirdeği doğrudan |
| JS / TS (wasm) | `client.on(FriendOnline, e => …)` (üretilmiş TS tipleri), `client.on("gap", …)` | Rust çekirdeği doğrudan |
| Bevy (istemci uygulaması) | `EventReader<Pushed<FriendOnline>>`; bir sistem çekirdeği `EventWriter`'a boşaltır | `gsb_client` + uygulamanın kendi eklentisi |

(Bevy satırı **istemci** uygulaması içindir; sunucu hiçbir yerde Bevy
motoru kullanmaz.) Boşluk algılama Rust çekirdeğindedir: her sarmalayıcı
aynı `Gap`'i alır, yeniden yazmaz.

## 9. Her kaybı say: sayaçlar

Her biri tek anlamlı (B32). Metrik aileleri `gsb_events_*_total` (motor
olayları `gsb_engine_events_*_total{subscriber}`), `gsb-metric
scope=events`, loadgen teli ve `RESULT` anahtarları (bir sonraki boş sihir
sayısı).

| Yer | Sayaçlar |
|---|---|
| Tutamaç (`Publisher`, atomik; yayıncı adıyla) | `events_pushed`, `events_refused_router_full`, `events_refused_router_gone` |
| Yönlendirici (registry) | `events_routed` (alınan itme), `events_offline` (satırı olmayan alıcı), `events_client_sent`, `events_client_parked`, `events_client_unsupported`, `events_client_refused_conn_full`, `events_client_refused_conn_gone`, `events_world_sent`, `events_world_no_room`, `events_world_refused_room_full`, `events_world_refused_room_gone`, `events_relocations`, `events_relocations_stale`, `events_seals_sent`, `events_seals_refused` |
| Bağlantı (`ConnSample`) | `pushes_written`, `pushes_dropped_outbound_full`, `pushes_dropped_outbound_gone` |
| Oda/shard tablosu | `events_admitted`, `events_no_handler`, `events_undecodable`, `events_taken`, `events_untaken` |
| Oda/shard yönlendirmesi | `events_world_not_here`, `events_forwarded`, `events_forward_refused`, `events_fence_held`, `events_fence_released`, `events_fence_overflow`, `events_fence_timeouts`, `events_fence_lost`, `events_relocations_refused`, `events_tombstones_expired` |
| Motor olayları (abone başına) | `engine_events_sent`, `engine_events_refused_full`, `engine_events_refused_gone` |
| İstemci (`gsb_client`, yerel) | `received`, `missed` (boşluk toplamı), `stale`, `restarted`, `undecodable`, `dropped_unread` |
| Tekrar | `replay_diverged` (mevcut) |

**Defter kimlikleri** (kimliğe adresli, tek bacaklı itmelerle spike
`losses::the_router_counts_every_leg_it_cannot_deliver` bunu kurar):
alıcı başına `offline` **ya da** her bacak için tam bir son —
CLIENT: `client_{sent, parked, unsupported, refused_conn_full,
refused_conn_gone}`; WORLD: `world_{sent, no_room, refused_room_full,
refused_room_gone}`. `client_sent = pushes_written +
pushes_dropped_outbound_* + yolda`. `world_sent = admitted + no_handler +
undecodable + not_here + forwarded + fence_overflow + (bekleyen)`;
`admitted = taken + untaken`.

## 10. Sıralama garantileri (düz söz)

1. **(Yayıncı, kimlik, bacak) başına FIFO** — evet: aynı yayıncının aynı
   kimliğe aynı bacaktan ittikleri itildikleri sırayla gelir; shard
   göçünde (çit, zincirli göç dahil) ve devamda da. İstisnalar adıyla
   sayılır: çit zaman aşımı (`events_fence_timeouts`).
2. Kayıp **yeniden sıralama değildir**: oda değiştirmede yoldaki olay
   kaybolur (sayılır), sonra gelmez.
3. Oda çapındaki olaylar: (yayıncı, oda) başına FIFO.
4. **Yayıncılar arasında sıra yok.** Banka ve mağaza aynı oyuncuya
   ittiyse odada hangisinin önce geleceği tanımsızdır.
5. **Bir BOTH olayın iki bacağı arasında sıra yok.**
6. **Olaylarla cevaplar/istekler arasında sıra yok** — farklı kanallar,
   farklı aktörler. Öneri: durum taşıyan olaylar **sürüm** taşır
   (`GoldReceived.version`, `BalanceChanged.version`); oda en yeniyi
   tutar. Banka hem alice'in cevabını hem bob'un olayını üretse de bob'un
   kendi çağrısının cevabı olayından önce ya da sonra gelebilir; sürüm
   kuralı ikisini de doğru birleştirir.
7. Tick içinde: EVENTS READ'den önce boşaltılır; tablo sırası boşaltma
   sırasıdır; `take::<E>` tip başına FIFO, `take_all` kanonik; tipler arası
   sıra sistem sırasıdır (SERVICES §5.7 ile aynı takas).
8. Motor olayları abone başına registry'nin işleme sırasıyla.

## 11. Determinizm

Olaylar odanın **kayıtlı girdileridir**. `determinism::Input`'un
`#[non_exhaustive]` yuvası (B178) dolar:

```rust
Input::Events(Vec<RecordedEvent>)   // dosya etiketi 18 (eklemeli; FORMAT_VERSION değişmez)
pub struct RecordedEvent { pub to: Option<Identity> /* None = oda */, pub owner: Owner,
                           pub publisher: u32, pub seq: u64, pub op: u16, pub payload: Bytes }
```

- Kaydedilen, tablonun **aldığıdır** (sahibi çözülmüş, `Input::Read` gibi);
  REPLAY onu aynı tabloya koyar — yönlendirici, yayıncı, servis yok.
  (Spike EVENTS'in boşalttığı listeyi kaydeder ve tekrar aynı EVENTS
  kodundan geçirir; tek odada ikisi eşdeğer. Motor "alınanı" kaydeder:
  shard'ların kaydı geldiğinde ileri gönderme/çit kayda girmesin — §20 E7.)
- Spike `replay::a_live_run_replays_to_the_same_hashes`: iki iş
  parçacığından titreşimli zamanlamayla 60 olay (`GoldReceived`,
  `BalanceChanged`) + girdiler + çağrı sonuçları, canlı oda kayıtlı; iki
  tekrar tick başına **aynı** hash'leri verir; negatif kontrol (bir
  tick'in olayları bir tick geç) tam o tick'te ayrışır.
- TEST modu (e5): `Sim`'e bir `Publisher` (`sim.publisher("bank")`),
  itme bir sonraki adımın EVENTS'inde görünür (ya da `Delay::ticks(n)`);
  `SimClient` PUSH karelerini ve boşlukları görür.
- Odanın itmeleri (oda yayıncıysa) çıktıdır, kaydedilmez; REPLAY'de
  tutamaç bağlantısızdır (§5.1). Motor olayları servislere gider,
  kaydedilmez.

## 12. Geri basınç ve kapasite

| Nokta | Sınır | Dolunca |
|---|---|---|
| Yönlendirici kutusu (e2'de registry kutusu) | registry kapasitesi | tutamaçta `RouterFull` (tipli, anlık) |
| Bağlantı aktörü kutusu | mevcut `ConnIn` kapasitesi | `client_refused_conn_full` |
| Bağlantının çıkışı | mevcut yazar kanalı | `pushes_dropped_outbound_full` |
| Oda/shard olay kutusu | `event_capacity` (öneri 1024) | `world_refused_room_full` |
| Çit bekletmesi | `HOLD_CAP` kimlik başına (256) | `events_fence_overflow` |
| Motor olay abonesi | abonenin kapasitesi | `engine_events_refused_full{subscriber}` |

- **`push_many` maliyeti:** yayıncıda O(1) (tek kodlama + kimlik
  dilimi); yönlendiricide O(N) (N satır araması + N `try_send`); bağlantı
  aktörlerinde alıcı başına O(gövde). 10 000 üyeli bir kanala saniyede bir
  satır = yönlendiricide saniyede 10 000 `try_send` — bir registry için
  fazla değil, ama join'lerle aynı kutuda.
- **Registry yükü — ölç, sonra böl:** e6'da loadgen'e sohbet/varlık
  senaryosu (N oyuncu, M kanal, saniyede K satır) eklenir ve şunlar
  ölçülür: registry kutu derinliği (yeni gösterge), join gecikmesi (p50/p99,
  itmeli ve itmesiz), `RouterFull` retleri, itme → yazma gecikmesi.
  **Bölme tetiği:** hedef yükte join p99'u itmesiz koşuya göre belirgin
  bozuluyorsa ya da `RouterFull` sıfır değilse → `EventRoutes` ayrı
  `EventRouter` aktörüne taşınır; registry ona olguları (Online/Joined/…)
  iletir; daha da gerekirse kimlik hash'iyle K yönlendirici (`push_many`
  yönlendirici başına bir mesaja bölünür, gövde paylaşılır).
- **Oda kutusu boyutu:** tick başına tepe olay sayısının ≥ 2 katı (s0'ın
  `N·k ≤ C` kuralının ikizi); `event_capacity` config doğrulamasında.

## 13. Güvenlik: kim neyi yayınlayabilir

- **Yalnız sunucu kodu yayınlar.** Yetki = bir `Publisher` tutmak;
  tutamaçları yalnız kompozisyon kökü üretir. İstemciden sunucuya PUSH
  yoktur: istemcinin yolladığı `PUSH` (17) karesi bağlantı aktöründe
  bilinmeyen/sunucu-yönlü opcode olarak **sert ihlal** sayılır (ihlal
  bütçesi, değişmedi).
- **İstemci olay yaptırmak isterse istek yollar** (`SendChat`): servis
  doğrulanmış kimliği `cx.player()`'dan alır (yükten asla), yetkiyi
  (kanalda mı, susturulmuş mu, hız sınırı) kendisi denetler, sonra iter.
  Yetkilendirme, içerik denetimi, hız sınırı oyunun işi (SERVICES §12).
- **Adresler doğrulanmış kimliklerdir** (bilet oyuncusu); anonim
  oturumun satırı yoktur. Yerel-auth yolunda kimlik istemcinin iddia
  ettiği addır — yalnız geliştirme yolu (devamdaki uyarının aynısı).
- **Oturum devralma:** aynı kimliğin yeni oturumu CLIENT bacağını
  devralır; eski oturuma bir şey gitmez.
- **Boyut:** tutamaçta `max_event_bytes` sınırı önerisi (§20 E3) — bir
  yayıncı hatası istemcinin kare sınırını aşan bir PUSH üretmesin.

## 14. Tel değişiklikleri (eklemeli — DESIGN §5)

```proto
// base.proto
message Auth {
  // … mevcut 1..3 …
  // Bu istemcinin okuyabildikleri (bit kümesi). Bit 0: PUSH kareleri.
  // Eski istemci göndermez (0) → ona asla PUSH gitmez.
  uint64 features = 4;
}

// Sunucu → istemci, opcode PUSH = 17 (güvenilir bant).
message Push {
  uint32 publisher = 1;   // PublisherId (0 = motor)
  uint64 seq = 2;         // (yayıncı, kimlik) başına CLIENT bacağı numarası, 1'den
  uint32 op = 3;          // olayın op'u (oyun bandı)
  bytes body = 4;         // olay gövdesi (yayıncıda bir kez kodlandı)
}
```

- **`PUSH = 17`**: temel bandın ilk boş numarası (15/16 sunucu içi
  işaretler, tele çıkmaz). **Güvenilir bant** (DESIGN §5.1 "kaybın
  bedeli"): kaybolan bir PUSH bir sonraki snapshot'la iyileşmez; sıralı
  akışta, `RpcResponse` karelerinin yanında. Kendi karesidir (oyunun
  `Private`'ının içinde değil), çünkü oda yolda yoktur.
- **Yetenek bayrağı:** `Auth.features` bit 0 (`FEATURE_PUSH`). Bağlantı
  aktörü AUTH'ta okur, `Authed { conn, features }` ile registry'ye taşır;
  bit yoksa CLIENT bacağı `events_client_unsupported` (spike `losses`).
  `PROTOCOL_VERSION` artmaz: eski istemci hiçbir yeni baytı görmez.
- Kilitler (e1): `wire_contract.rs`'e `Push` alan numaraları ve
  `Auth.features = 4`'ün eklemeli olduğu.
- (§20 E5 önerisi kabul edilirse `uint64 stream = 5`: satırın enkarnasyonu.)

## 15. İstemci ve FFI

**Rust (`gsb_client::events`, sans-IO — spike `src/client.rs`):** istemci
döngüsü her kareyi `events.ingest(&frame)`'e verir (PUSH ise tüketir —
`requests`'in çıkarıcısı gibi, ama PUSH temel bant karesi olduğu için oyunun
çıkarıcısına gerek yok). Yayıncı başına son numara: `seq > son + 1` →
`Gap { from, missed }`; `seq == 1` yeni bir akış (`restarted`); `seq ≤
son` → `stale` (bu tasarımda olmaz; olursa sayılır). Dinleyicisi olan
olay hemen çağrılır, olmayan sınırlı kuyruğa girer (`take::<E>()`; taşan
en eski düşer, `dropped_unread`). `on`/`take`/`take_gaps`.

**FFI (`ffi.proto`, telde değil):**

```proto
message Event {
  oneof kind {
    // … B28'in kolları (cevap, oda olayları, kare; SERVICES §14) …
    Push push = N;     // { uint32 publisher; uint64 seq; uint32 op; bytes payload }
    Gap  gap  = N + 1; // { uint32 publisher; uint64 missed }
  }
}
```

Boşluk algılama çekirdekte (Rust) yapılır; sarmalayıcı `Gap`'i bir olay
olarak alır. C# sarmalayıcısı op → ayrıştırıcı tablosunu ve tipli olay
yüzeyini `.proto`'dan üretir:

```csharp
public sealed partial class GameEvents {
    public event Action<FriendOnline>  FriendOnline;
    public event Action<ChatMessage>   ChatMessage;
    public event Action<BalanceChanged> BalanceChanged;
    public event Action<uint, ulong>   Gap;              // publisher, missed
    internal void Dispatch(FfiPush p) {
        switch (p.Op) {
            case 2002: ChatMessage?.Invoke(global::Game.ChatMessage.Parser.ParseFrom(p.Payload)); break;
            case 2003: FriendOnline?.Invoke(global::Game.FriendOnline.Parser.ParseFrom(p.Payload)); break;
            case 2005: BalanceChanged?.Invoke(global::Game.BalanceChanged.Parser.ParseFrom(p.Payload)); break;
            // WORLD-yalnız olaylar (GoldReceived) burada yok: istemciye hiç gelmezler
        }
    }
}
// Poll() (MonoBehaviour.Update) Event.Push → game.Events.Dispatch, Event.Gap → game.Events.Gap
```

## 16. Gözlemlenebilirlik

- Sayaçlar §9 (Prometheus, OTLP, `gsb-metric`, loadgen `RESULT`).
- Registry kutu derinliği göstergesi (bölme kararının girdisi, §12) ve
  örneklenmiş itme → yazma gecikmesi histogramı (publisher damgası
  bağlantı aktöründe okunur; telde değil).
- İz: `Publisher::within(cx)` (servis çağrısının içinden) itmeye çağrının
  iz kimliğini bağlar; yönlendirici `gsb.push` debug span'i
  (`publisher`, `op`, `recipients`, `trace`) yayar — "bu sohbet satırı
  hangi istekten çıktı" sorusu. Tel alanı yok.
- Retler hız sınırlı uyarı günlüğü (yayıncı adıyla).

## 17. İsteklerle etkileşim

- **Bir istek, iki taraf:** altın aktarımında (§8.1) alice'in isteği
  `.answers` ile bankaya gider; banka alice'e **cevap** döner (istek
  yolu, RESPONSES), bob'a **olay** iter (olay yolu, EVENTS). İkisi ayrı
  yollardır, aralarında sıra yoktur; sürüm numarası birleştirir.
- **Göç ve yeniden yayın:** göçte çağrı aynı anahtarla yeniden yayınlanır
  (SERVICES §8); servis "ilk kez mi"yi defterden alıp ikinci kez
  itmez (tek etki → tek olay).
- **Olay → çağrı:** oda bir olayı alıp çağrı yapabilir (`QuestDone` →
  `inv.grant(..).on_behalf_of(&identity).owner(e.owner)`); sonuç SERVICES
  yolundan gelir.
- **`forward` + itme:** sohbet gibi oda dışı akış tamamen servisler
  arasında kalır: istek `forward` → servis → `push_many`. Oda ne isteği
  ne olayı görür.
- **Cevap mı olay mı:** isteyene söylenen şey **cevaptır** (tek, korelasyonlu,
  zaman aşımlı); başkasına ya da isteksiz söylenen **olaydır**. İsteyene
  ek bir olay da itilebilir (ör. "başarım açıldı"), cevaba göre sırası yok.

## 18. Spike: `spikes/e0-events`

gsb çalışma alanının üyesi değil (kök `exclude`), tek crate'lik kendi
çalışma alanı; motora dokunmaz. Aktörleri kendisi modeller (sınırlı tokio
kanalları; `try_send`/`try_recv` çalışma zamanı istemez; canlı testlerde
tek `await`'li döngüler). `build.rs` `gsb-lint`'i koşar.

| Modül | Ne |
|---|---|
| `contract` | `Dest`, `Event`/`WorldEvent`/`ClientEvent`, `WireMessage`, kimlikler |
| `wire` | `PUSH = 17`, `Push`, `FEATURE_PUSH` |
| `game` | örnek olaylar + `gsb-build`'in yazacağı impl'ler |
| `codegen` | `(gsb.event)` okuyan tanımlayıcı aynaları, denetimler, üretim |
| `publisher` | `Publisher`, `Target`, `PushRefused` |
| `router` | yönlendirici (registry'nin tablosu): satırlar, bacaklar, numaralar, `Relocated`/seal, motor olayları |
| `conn` | bağlantı aktörünün PUSH kolu |
| `room` | oda/shard: CONTROL → RESPONSES → EVENTS → READ → SYSTEMS → END; mezar taşı, ileri gönderme, çit; kayıt ve tekrar |
| `events` | `Events` tablosu (`accept`, `take`, `take_all`, tick sonu) |
| `client` | istemcinin olay yarısı (`on`, `take`, boşluklar) |
| `engine` | `EngineEvent`, `Interest`, aboneler |
| `harness` | adım adım sürülen küçük sunucu + `Wallets` oyunu |

Çalıştırma: `cd spikes/e0-events && cargo test` (23 test + 1 doc testi +
5 `compile_fail`).

| Gereksinim | Test |
|---|---|
| Kimlikle yönlendirme, başka oda | `routing::gold_reaches_bob_in_another_room` |
| Shard göçünde FIFO (çit) / çitsiz yeniden sıralama (negatif kontrol) | `routing::a_migration_keeps_per_publisher_fifo_with_the_fence`, `…without_the_fence_a_migration_reorders` |
| Zincirli göç | `routing::a_chained_migration_keeps_fifo_too` |
| Park/devam: CLIENT parkta sayılır, WORLD teslim, istemci boşluğu görür | `routing::park_and_resume_keep_routing_and_the_client_sees_its_gap` |
| Oda değiştirme kaybı sayılır | `routing::leaving_between_route_and_drain_is_counted_not_here` |
| Kendi kanal yalıtımı (spam yapan istemci olay kaybetmez) | `paths::a_spamming_client_does_not_lose_its_events` |
| Faz sırası (kayıttan) ve READ'den önce EVENTS'in anlamı | `paths::the_phase_order_is_control_responses_events_read`, `…gold_arriving_this_tick_pays_for_a_purchase_this_tick` |
| CLIENT doğrudan yolu odayı atlar | `paths::a_client_event_never_touches_a_room` |
| BOTH, bacak başına numara | `paths::both_runs_both_legs_with_their_own_numbers` |
| `push_room` / `push_all` | `paths::push_room_and_push_all_address_rooms_and_sessions` |
| `push_many` bir kez kodlar, gövdeyi paylaşır | `paths::push_many_encodes_once_and_shares_the_body` |
| Tutamaç retleri tipli; yönlendiricinin her kaybı adıyla + defter | `losses` (4) |
| Motor varlığı → servis → arkadaşlar (canlı); dolu abone sayılır | `presence` (2) |
| Kayıt/tekrar aynı hash'ler; negatif kontrol | `replay` |
| Boşta maliyet yapısal | `idle` |
| `(gsb.event)` biçimi protoc'tan geçer; denetimler; üretilen metin | `codegen` (2) |
| Yanlış taraf / yanlış tip derleme hatası | `src/lib.rs` doc testleri (E0277 ×4, E0631) |

**Bulgular:**

1. **Göçte yeniden sıralama gerçek** (negatif kontrol 4,5,6,1,2,3);
   çit onu registry'de tampon olmadan çözer: seal odanın olay FIFO'sunda,
   fence shard FIFO'sunda yol alır.
2. **Zincirli göç bir kural daha ister** (§7.3): gelen çitini bekleyen
   shard kendi doğrudan bacaklarını bekletir, ileri gönderilmişleri hemen
   aktarır; giden çit gelen çitten sonra. İlk taslak (bekletilenleri
   göçte hemen ileri göndermek) iz sürülürken düştü: 3, 4 numaralılar
   C'ye, A'nın hâlâ B üzerinden ilettiği 1, 2'den önce varır. Mutasyonla
   doğrulandı: o taslağa dönünce test `[3, 4, 1, 2, 5]` görür.
3. **Numara bacak başına olmalı** — yoksa istemci WORLD bacaklarını
   boşluk sanar.
4. **EVENTS sahibi çözmek için kimlik → oyuncu eşlemi ister**; odada
   bugün `ConnId → PlayerId` var, kimlik join'de geliyor: küçük bir indeks.
5. `option (gsb.event) = { op: 2001, to: CLIENT }` vendored protoc'tan
   geçer; tanımlayıcıda ileti-tipli uzantı (50004) kırpılmış aynayla
   okunur — q3'ün yöntemi aynen işler.
6. Hedef işaretçileri derleme zamanında yakalar: E0277 (yanlış taraf,
   olay olmayan tip), E0631 (yanlış gövde tipli dinleyici).
7. READ'den önce EVENTS, aynı tick'teki girdiye aynı tick'in olayını
   gösterir — oyun sistemlerinde olayları önce alırsa.
8. Canlı titreşimli koşunun kaydı iki kez aynı hash'lere tekrar eder;
   kayıt EVENTS'in sırasını taşıdığı için zamanlama kayıttan gelir.

**Kanıtlayamadıkları:** gerçek registry/bağlantı/oda/shard aktörleri
(`RoomActor`'ın fazları, `ShardMsg`, ertelenmiş göçler, devam yayını);
gerçek tel baytları (TCP/rUDP); `gsb-build`'in kendisi (spike'ın ayrı bir
geçişi var); `#[gsb(engine_events)]` makro kolu (spike döngüyü elle
yazar); bevy_ecs/hecs biçimleri (yalnız belge — `Calls` adaptörleriyle aynı
desen); yük altında yönlendirici/registry maliyeti; dosya biçimi
(etiket 18); FFI ve sarmalayıcılar; süreçler arası yönlendirme.

## 19. Elenen alternatifler

| # | Alternatif | Neden elendi |
|---|---|---|
| E-A1 | WORLD bacağını üyenin girdi kanalından göndermek (s0) | Spam yapan istemci kendi olaylarını düşürür; olaylar girdi bütçesini paylaşır; oturumsuz (parktaki) varlığa ulaşamaz. Karar 2. |
| E-A2 | Sharded odada tüm shard'lara yayın + "sahibi alır" | Geçişteki olay hiçbir shard'a ait olmaz ve sayılamaz (N shard "bende değil" der); N kat iş. |
| E-A3 | Odasız oyuncu için hub | s0b'de kalktı; lobi bir odadır, odasız WORLD bacağı sayılır (`world_no_room`). |
| E-A4 | Motor içi konu/pub-sub | Üyelik politikası oyunundur; motor yalnız kimlik/oda/herkes bilir. Sonra bir kit yapı taşı olabilir. |
| E-A5 | En-az-bir-kez + onay + çevrimdışı kutu motorda | Depolama ve yeniden deneme oyunun işi; motor en-fazla-bir-kez + sayaç + boşluk numarası verir, istemci yeniden eşitler. |
| E-A6 | Alınmayan olayı bir tick tutmak | Gizli kuyruk, hata maskeleme, tick'ler arası kayıt durumu (§4.4). |
| E-A7 | Hedefi itme anında argüman olarak vermek (s0 `Dest::Client`) | Hedef tipin özelliği; derleme zamanı denetimi ve `.proto`'dan diğer dillere üretim ancak böyle. |
| E-A8 | Olayları `Calls` sonuçları olarak teslim etmek | Olay bir çağrının cevabı değildir: anahtarı, zaman aşımı, sahibi yok; sayaçlar karışırdı. Aynı biçimde ayrı bir tablo (ikiz). |
| E-A9 | Numarayı yayıncı tutamacının damgalaması | Tutamaç başına sınırsız kimlik haritası; `push_many` N damga; yönlendiricideki kayıplarla tutarlı boşluk vermez. Yönlendirici zaten her alıcıyı görür. |
| E-A10 | İki bacağa tek numara | İstemci WORLD bacaklarını boşluk sanar (spike bulgusu 3). |
| E-A11 | Yayıncılara odaların olay kutularını doğrudan vermek | Konumu yayıncı bilmeli → göçte bayat; çevrimdışını sayan tek yer kalmaz. |
| E-A12 | Göçte kaynağın olayı registry'ye geri göndermesi | Ek tur, kontrol düzlemine ek yük; shard FIFO'su yerel ve sıralı. |
| E-A13 | Çitsiz (yeniden sıralama penceresini kabul) | FIFO garantisi göçte düşer (negatif kontrol); çitin bedeli göç başına bir seal turu. |
| E-A14 | İstek ve olaya ayrı op uzayları | İki numaralandırma; kayıt/günlük/dağıtıcıda belirsizlik; tek bant denetimi daha basit. |
| E-A15 | Motorun varlığı istemcilere doğrudan itmesi | Motor "arkadaş"ı tanımlar olurdu; karar: motor olayları servislere, servis iter. |
| E-A16 | CLIENT olaylarını odadan geçirmek (`Private` karesinde) | Sohbet odanın tick'ine ve varlığına bağlanır; odasız oyuncuya ulaşamaz; odaya maliyet. |
| E-A17 | Yönlendiriciyi baştan ayrı aktör(ler) yapmak | Karar 3: önce registry (olgular orada); tablo ayrı struct olduğu için ölçüm isterse bölmek kodu değiştirmez. |

## 20. Açık sorular (her biri öneriyle)

1. **E1 — Parktaki kimliğe WORLD bacağı.** Teslim (varlık odada, sahibi
   yaşıyor; banka bakiyesi parktaki karaktere yazılır) mı, s0'daki gibi
   düşür + say mı? **Öneri: teslim** (spike böyle); CLIENT bacağı
   `client_parked` sayılır.
2. **E2 — Yönlendiricinin yeri.** e2 registry'de (karar). **Öneri:**
   `EventRoutes`'ı registry kodundan ayrı bir struct olarak yaz; e6'daki
   loadgen ölçümünden önce bölme.
3. **E3 — Boyut sınırı.** **Öneri:** tutamaçta `max_event_bytes`
   (varsayılan, aktarımın kare sınırının altında, ör. 64 KiB) →
   `PushRefused::TooLarge` + `events_refused_too_large`.
4. **E4 — `push_many`'de yinelenen kimlik.** **Öneri:** tekilleştirme yok
   (yayıncının listesi; iki numara ikisini de görünür kılar); belgele.
5. **E5 — Yeni oturumda numara sıfırlanması.** Spike `seq == 1`'i "yeni
   akış" sayar (yeni akışın ilk olayı kaybolursa yanılır). **Öneri:**
   `Push.stream = 5` (satır enkarnasyonu, eklemeli) — istemci akış
   değişince sıfırlar; kesin.
6. **E6 — Kanca biçimi.** `update_with_events(world, ctx, &mut Calls, &mut
   Events)` mı, tabloları demetleyen tek `Io` mu? **Öneri:** şimdi eklemeli
   kanca (q1'in tarzı); üçüncü tablo gelirse `Io`.
7. **E7 — Ne kaydedilir.** Boşaltılan liste mi, tabloya alınan mı?
   **Öneri:** alınan (sahibiyle, `Input::Read` gibi); shard kaydı geldiğinde
   ileri gönderme/çit kayda girmez.
8. **E8 — Çit sabitleri.** `TOMB_TTL`/`FENCE_TTL` 64 tick, `HOLD_CAP` 256.
   **Öneri:** sabitle başla, ölçüm isterse config.
9. **E9 — Sharded odaya oda çapında WORLD.** Her shard'a mı, yalnız 0'a
   mı? **Öneri:** her shard'a (oda çapındaki kural her yerde geçerli).
10. **E10 — Odalar yayıncı mı?** **Öneri:** evet, aynı `Publisher`
    (bir oda başka odadaki oyuncuya "lonca üyen düştü" der); REPLAY'de
    bağlantısız tutamaç.
11. **E11 — BOTH bacaklarının sırası.** Garanti yok. **Öneri:**
    yönlendirici WORLD'ü önce gönderir (çoğu zaman oda istemciden önce
    görür) ama bu belgelenmiş bir garanti değildir.
12. **E12 — Varlığın yeniden kurulması.** Servis yeniden başlarsa kim
    çevrimiçi? **Öneri:** registry'ye `Handle`'lı bir sorgu
    (`online_identities()` sayfalı) ilk ihtiyaçta; motor geçmiş tutmaz.
13. **E13 — Oda değiştirmede yoldaki olay.** Sayılır ve kaybolur.
    Alternatif: registry'de "bekleyen join" süresince tampon. **Öneri:**
    say; yeni oda durumu zaten yükler.
14. **E14 — Yetenek: bit kümesi mi sürüm mü.** **Öneri:** bit kümesi
    (`Auth.features`), eklemeli; `PROTOCOL_VERSION` yalnız yanlış
    ayrıştırmada artar (DESIGN §5.5).
15. **E15 — İstemci kuyruğu.** Dinleyicisiz olaylar için 1024'lük sınır.
    **Öneri:** config (`EventsClient::with_capacity`), varsayılan 1024.

## 21. Uygulama planı

**e1 — sözleşme ve tel (yönlendirme yok).** `gsb_protocol::event`
(`Event`, `WorldEvent`, `ClientEvent`, `Dest`); `gsb/options.proto`'ya
`Destination`, `EventOptions`, `(gsb.event)` 50004, `(gsb.reserved_ops)`
50005; `gsb-build`: olay planı, tek op uzayı denetimleri, üretim
(`impl Event` + işaretçiler); `base.proto`: `Push`, `Auth.features = 4`,
`op::base::PUSH = 17` (+ `wire_contract.rs` kilitleri). Demo/kit
`.proto`'suna bir örnek olay.

**e1 — uygulanan biçim (2026-10-08).** Plandaki her madde girdi;
tasarımdan farklar ve netleştirmeler:

- **`FEATURE_PUSH` bir `.proto` enum'u:** `base.proto`'da `enum Feature
  { FEATURE_NONE = 0; FEATURE_PUSH = 1; }` — değerler bit **maskesi**
  (1, 2, 4 …), indis değil; her istemci dili sabiti üretir (ErrorCode'un
  gerekçesi). Rust'ta `gsb_protocol::FEATURE_PUSH: u64 = 1`
  (`Feature::Push`'tan). Bilinmeyen bitler yok sayılır.
- **`Push.stream = 5` (§20 E5):** satırın enkarnasyonu; aynı `stream`
  içinde `seq` boşluk gösterir, `stream` değişince istemci tüm
  yayıncıların sayacını sıfırlar. Yorum "gönderilen itmede asla 0"
  der — e2 akış numarasını 1'den başlatmalı.
- **`Push` taban tabloda kayıtlı** (`base_table()`): sunucu kodlar,
  istemci çözer. İstemciden gelen 17 yine sert ihlal: bağlantı aktörü
  taban bandındaki bilinmeyen gelen op'u tablo sorusundan önce reddeder
  (değişmedi).
- **Bugünkü istemci `features: 0` gönderir** (`gsb_client::session`):
  PUSH okuyamadığı için biti e6'da açar.
- **`gsb-build`:** olaylar servis kancasının dosya başına `finalize`'ında
  yazılır (prost-build'in mesaj kancası yok); `finalize` çağrı sayısı
  dosya sayısına eşit değilse build reddedilir (impl'ler sessizce
  düşmesin). Ret adları: istekler `pkg.Servis.Metot`, olaylar
  `pkg.Mesaj`; aynı op'ta önce istekler sahiplenir. Yeni `Problem`
  kolları: `EventMissingOp`, `EventMissingDestination` (`op: 0` ve
  `DESTINATION_UNSPECIFIED` proto3'te kodlanmaz, eksik sayılır),
  `EventEngineRange`, `EventTooLarge`, `EventReservedOp`,
  `EventIsRequest`; `DuplicateOp` iki uzayı birlikte kapsar. İç içe
  mesajda olay desteklenir (`outer::Inner`). Olay sözleşmesinin yolu
  `Builder::event_contract_path` (varsayılan `::gsb_protocol::event`).
  `gsb_core::events` yeniden dışa açımı e2'ye kaldı (orada `Events`
  tablosuyla birlikte).
- **Demo:** `ItemGranted { kind, version }` (op 1007 = `gsb_demo::op::
  ITEM_GRANTED`, `to: BOTH`); kilit `wire_contract.rs`
  (`the_generated_event_contract_matches_the_demo_ops`).
- **Kilitler** (`gsb-demo/tests/wire_contract.rs`): `Push`'un beş alan
  numarası, `PUSH = 17`, `FEATURE_PUSH = 1`, `PROTOCOL_VERSION = 1`,
  `Auth.features = 4`'ün eklemeli olduğu (alanı bilmeyen okuyucu çözer;
  `features = 0` baytları değiştirmez). Mevcut kilitler değişmedi.

**e2 — çekirdek yönlendirme.** `gsb_core::events` (`Events` tablosu,
`Publisher`, `Push`, `PushRefused`, sayaçlar); registry'de `Identity →
Row` + itme işleme + `Authed.features`; oda ve shard'da olay kutusu
(`RoomConfig::event_capacity`) ve EVENTS fazı (ortak kod, istekler gibi
`Parts`); odada kimlik → oyuncu indeksi; `GameLogic::{boot_events,
update_with_events}`, `ShardLogic::update_seam_with_events`;
`ConnIn::Push` + PUSH yazımı; `RegistryParts::publisher`; sayaçlar →
metrikler (satır, Prometheus, OTLP, loadgen teli, `RESULT`). Kapılar:
hedefli testler ajanda, tam set ebeveynde.

**e2 — uygulanan biçim (2026-10-08).** Plandaki her madde girdi;
tasarımdan farklar ve netleştirmeler:

- **Yönlendirme tablosu türetilmiş durum okur (E2'nin biçimi).** Ayrı bir
  `Row { session, parked, place }` kopyası yok: `EventRoutes`
  (`gsb_core::events::route`) yalnız yönlendirmenin kendi durumunu
  tutar — (yayıncı, kimlik, bacak) numaraları, satırın `stream`'i, oda
  çapındaki (yayıncı, oda) numaraları ve sayaçlar — ve oturumu, parkı,
  konumu registry'nin kendi tablolarından dar bir `Locate` trait'i
  üzerinden okur (`registry/actor/events.rs`: kimlik → satırlar indeksi
  + `ConnInfo` + `RoomEntry.events`). Gerekçe: iki kopyanın tutarlılık
  işi yok (park anahtarına taşıma, `LeaveConn`, devralma, `RoomDied`
  hepsi zaten `ConnInfo`'yu güncelliyor). Ayrı bir `EventRouter`
  aktörüne bölmek (§12) = `Locate`'i registry'nin ilettiği olgular
  üzerinde uygulamak; yönlendirme kodu değişmez. Satırların tek silme
  yolu `release_row`; kimliğin son satırı gidince numaraları biter
  (çevrimdışı → yeni oturum yeni `stream`, 1'den).
- **CLIENT bacağı** kimliğin en yeni canlı (doğrulanmış, ayrılmamış,
  gelen kutusu olan) oturumuna gider; yoksa satır varsa
  `events_client_parked`. **WORLD bacağı** üyeliği olan satıra (canlı
  önce, sonra parktaki — E1: parktakine teslim); sharded odada
  `ConnInfo.shard` (yeni: `SpawnDone.shard` — katılımı alan shard: ev
  shard'ı ya da parkı devam ettiren) shard'ın kutusuna. Göç e3'e kadar
  izlenmez: göçmüş üyenin bacağı eski shard'da `events_world_not_here`.
- **`Authed` kimliği de taşır** (`Authed { conn, identity, features }`):
  kimlik katılımdan ÖNCE bilinmeli ki odasız oyuncuya CLIENT bacağı
  gitsin; `features` (`Auth.features`) satıra yazılır.
- **EVENTS fazının yeri:** RESPONSES'tan sonra, CONTROL tarafının
  ayrılma/park süpürmelerinden (0c/0d) sonra, READ'den hemen önce
  ("faz 0e"): üyelik o tick için kesinleşmiş olur. Faz tek yerde yazılı
  (`events::Inbound::drain`), oda ve shard aynı kodu çağırır; tick başına
  en çok bir kutu dolusu boşaltır; kimlik → üye indeksi yalnız oyuncuya
  adresli bacak varsa, tick başına bir kez, üye tablosundan kurulur
  (ayrı bir indeksin bakımı yok, tabloyla çelişemez). Sahip: oturumun
  açık bağı (`calls.bound`), yoksa `GameLogic::call_owner`; `None` →
  yeni sayaç `events_unbound`. Oda çapı bacak `Owner::ACTOR`.
- **Ek sayaçlar (§9'da yoktu):** `events_unbound` (oda; sahipsiz üye),
  `events_unread` (registry durduğunda kutusunda kalan itme — her
  bacağı birden kaybolur), `pushes_dropped_unread` (oturum biterken
  bağlantı aktörünün gelen kutusunda kalan CLIENT bacağı). Defter:
  `world_sent = admitted + no_handler + undecodable + unbound +
  world_not_here` (+ yolda), `client_sent = pushes_written +
  pushes_dropped_outbound_{full,gone} + pushes_dropped_unread` (+ yolda),
  `Σ events_pushed = events_routed + events_unread` (+ yolda). Çit
  sayaçları (`relocations*`, `seals*`, `events_forwarded` …) e3'te.
- **`BootError`:** `EventOpCollision { op, first, second }` (başka bir
  olay ya da çağrı tablosunun bir isteği aynı op'u alıyor — tablo
  `Calls`'ın op'larını kopyalar, `boot_events` `boot_calls`'tan sonra
  koşar) ve elle yazılmış `impl Event` için `EventEngineOp`. Kurulamayan
  tablo odayı reddeder (B153 yolu).
- **`Publishers`** (`gsb_core::events`): sunucu başına darphane;
  `RegistryParts::publisher(name)` onu sarar. Kimlikler 1'den (0 motor);
  aynı ad aynı tutamaç (bir kimlik, bir sayaç kümesi).
  `max_event_bytes` (varsayılan 64 KiB, config) tutamaçta, kodlamadan
  sonra denetlenir (`TooLarge { size, max }`).
- **Bağlantı aktörü** `PUSH`'u `try_send` ile yazar (beklemez);
  `FEATURE_PUSH` denetimi yönlendiricidedir (`events_client_unsupported`).
  Bir istemcinin kendi gelen kutusunu doldurması kendi CLIENT
  bacaklarını kaybettirir (`events_client_refused_conn_full`) — WORLD
  bacağı odanın kutusunda olduğundan etkilenmez (§3.1 yalıtımı WORLD
  içindir).
- **Metrikler:** oda satırında olay tablosu anahtarları (çağrılarınkinden
  sonra), yeni `scope=events` satırı (yönlendirici + PUSH kareleri) ve
  yayıncı başına `scope=publisher`; aileler `gsb_room_events_*_total`,
  `gsb_events_*_total`, `gsb_net_pushes_*_total`,
  `gsb_publisher_events_*_total{publisher}`. Loadgen teli genişledi
  (sihir GSNW'ye birleşmede geçer); `RESULT`'ta olay anahtarları ve
  `pub_<ad>_<sayaç>=`.
- **Kalanlar** (BACKLOG `e2:`): kit sarmalayıcıları `boot_events`/
  `update_with_events`'i iletmiyor (e5); kayıt/tekrar olayları
  taşımıyor (e5); `push_room` üyeleri bağlantı tablosunu tarayarak
  bulur.

**e3 — konum değişiklikleri.** Shard göçü: `PlayerMigration.mig`,
`Relocated`, seal, `ShardMsg::{Forward, Fence}`, mezar taşları, bekletme,
zincir kuralı; park/devam/oda ölümü yolları; gerçek shard aktörleriyle
testler (q5'in `calls_actors.rs` deseni).

**e4 — motor olayları.** `EngineEvent`, `Interest`, aboneler;
`gsb-macros`: `#[gsb(engine_events)]` kolu + denetimler;
`Setup::engine_events`, `RegistryParts::engine_events`; demoda bir
arkadaş servisi.

**e5 — determinizm ve bağlayıcılar.** `Input::Events` (etiket 18), tekrar,
TEST modunda `sim.publisher(..)`; `gsb-bevy-ecs::events` (`GsbEvents`,
`lend`), `gsb-hecs`; `gsb-conformance`'a olaylı senaryo (üç biçim, aynı iz).

**e6 — istemci ve ölçüm.** `gsb_client::events` (sans-IO, boşluk
algılama, `features` bitini AUTH'ta gönderir); loadgen sohbet/varlık
senaryosu ve registry yükü ölçümü → bölme kararı (§12). Sonra istemci
çekirdeği turunda (B28) FFI `Event::{Push, Gap}`, C#/Godot/JS üretilmiş
olay yüzeyleri.
