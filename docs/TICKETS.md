# gsb: İmzalı Biletler — gerçek bilet doğrulaması (B21)

Statü: **yapıldı (t21 turu, 2026-10-03).** Kod: `crates/gsb-ticket`
(opt-in crate), çekirdekte sayım ve talep taşıma (`gsb_core::auth`),
`gsb-client`'in `grant` feature'ı, `gsb-server`'ın `ticket` feature'ı
(`[ticket]` tablosu), uçtan uca örnek `examples/lobby`.

## 1. İlke: motor biçimden bağımsız, yapı taşı opt-in

Gerçek bir dağıtım **devretmeyle** kimlik doğrular: platform (lobi,
eşleştirme) maçı kurar, istemciye bir bilet ve oyun sunucusunun adresini
verir; oyun sunucusunun işi **platformun verdiği bileti doğrulamaktır**
(RPC-CONTROL-PLANE §7). Motorun bunun için **tek** dikişi vardır:
`gsb_core::auth::TicketAuth` (bilet baytları → `ValidatedTicket`,
zaman aşımlı, bağlantı aktörünün worker'ında, tick dışında). Bu tur o
dikişi değiştirmedi; yanına ikisi eklendi (§8, §9):

- **Sayım:** her AUTH kararı tek bir sebeple sayılır.
- **Talepler:** doğrulayıcının doğruladığı oyuna özel talepler
  (`ValidatedTicket::extra`) oyunun katılım kancalarına kadar taşınır.

Bir oyun `gsb-ticket`'ı hiç kullanmayabilir: kendi auth servisini, Steam
oturum biletini, … kendi `TicketValidator` kapanışıyla takar; sayım ve
talep taşıma onun için de çalışır (`TicketError::Refused(reason)`,
`ValidatedTicket::new(..).with_extra(..)`). `gsb-ticket` hazır bir
cevaptır, zorunluluk değil: ne `gsb-core` ne `gsb-server` varsayılan
olarak ona bağımlı.

## 2. Akış

```text
istemci                lobi (platform)              oyun sunucusu
   │── giriş (HTTPS) ──────▶│                               │
   │                        │ hesap → karakter, sınıf …     │
   │                        │ Claims<T> → PASETO v4.public  │
   │◀── JoinGrant ──────────│  (yayıncının Ed25519 anahtarı)│
   │  {kapılar, rUDP anahtarı, bilet, oda}                  │
   │── rUDP el sıkışma (sunucu anahtarı İZİNDEN sabitlenir) ▶│ Noise NK
   │── AUTH {bilet} ────────────────────────────────────────▶│ worker: imza, talepler,
   │                                                         │ oyunun kontrolü, (tek kullanım)
   │◀── AUTH_RESULT {player, room} ya da ERROR 10 ───────────│ sayım: tickets_*
   │── JOIN_ROOM {room} ────────────────────────────────────▶│ registry → oda/shard
   │                                                         │ on_join_verified(Joiner{kimlik, talepler})
   │◀── JOIN_ROOM_RESULT, anlık görüntüler ──────────────────│
```

Yeniden bağlanma (RECONNECT §5): yeni soket aynı izinle aynı bileti
sunar; bilet süresi içindeyse (varsayılan politika, §6) kabul edilir ve
JOIN örtük resume'dur.

## 3. Biçim kararı: PASETO v4.public

**Seçilen:** PASETO v4.public — Ed25519 imza, JSON talepler, anahtar
kimliği (`kid`) imzalı altbilgide: `v4.public.<base64url(m ‖ imza)>.<base64url({"kid":…})>`.

Gerekçe:

| Ölçüt | PASETO v4.public | Kendi ikili biçimimiz | JWT (JWS) |
|---|---|---|---|
| Başka dilde basma | Standart; Node'un yerleşik Ed25519'u + 20 satır (§12) ya da bir PASETO kütüphanesi | Her platformda yeniden yazılır, test vektörü yok | Yaygın |
| Yanlış kullanıma direnç | Sürüm/amaç başlıkta sabit, `alg` alanı yok; PAE ile altbilgi ve örtük iddia da imzalı | Bizim tasarımımız kadar | `alg` karışıklığı (`none`, HS/RS), tarihçesi kötü |
| Boyut | ~300 B (tek AUTH karesinde bir kez — önemsiz) | ~100 B | ~350 B |
| Bakım | Spesifikasyon + resmi test vektörleri | Bizde | Kütüphaneye bağlı |

Simetrik `v4.local` elendi: anahtar her oyun sunucusunda olurdu, bir
sunucu bilet basabilirdi; açık anahtarla doğrulamada sunucu yalnız
doğrular. Lobi Nuxt/TypeScript olacağı için birlikte çalışabilirlik
belirleyici oldu.

**Uygulama:** hazır PASETO crate'i (`pasetors`, `rusty_paseto`) yerine
~140 satırlık kendi v4.public katmanımız (`gsb_ticket::paseto`): PAE,
imza, ayrıştırma. Neden: lock'ta zaten olan `curve25519-dalek 4`
(snow üzerinden) üstündeki `ed25519-dalek 2.2` ile (3.x ikinci bir
curve25519-dalek çekerdi), `verify_strict` ile; ek bağımlılık en az.
Doğruluğun kanıtı **resmi test vektörleri**: 4-S-1, 4-S-2, 4-S-3 birebir
aynı belirteci imzalıyor ve doğruluyor; 4-F-1, 4-F-2, 4-F-3 (başka
sürüm/amaç, başka anahtar) reddediliyor; spesifikasyonun PAE örnekleri
tutuyor (`paseto/tests.rs`).

**Bağımlılık kanıtı:** saf Rust; `ring`, `aws-lc` ya da C yok. Lock'a
eklenen ve DERLENEN: `ed25519-dalek 2.2.0`, `ed25519 2.2.3`,
`signature 2.2.0`, `sha2 0.10.9`, `serde_json 1.0.151` (+ `itoa`,
`zmij`). Lock'ta görünen ama hiçbir şeyin çekmediği isteğe bağlı
girdiler: `pkcs8`, `der`, `spki`, `const-oid`, `base64ct`,
`rand_core 0.6` (`cargo tree --workspace -i pkcs8` boş).
`cargo tree -p gsb-ticket -e normal`'da ring/aws/cc yok; lock'taki
`ring` yalnız önceden var olan rustls/quinn yolundan.

## 4. Talepler

| Talep | Zorunlu | Anlam |
|---|---|---|
| `sub` | evet | Oyuncu kimliği → `ValidatedTicket.player`: resume anahtarı, K4 ev shard'ı/karakter anahtarı. 1–128 bayt |
| `room` | evet | Biletin sabitlediği oda (sayı; JS'te 2^53'e kadar güvenli) |
| `aud` | evet | Biletin basıldığı alan/sunucu; doğrulayıcınınkiyle aynı olmalı |
| `iat` | evet | Basılma zamanı (RFC 3339) |
| `exp` | evet | Son geçerlilik (RFC 3339); kısa — dakikalar |
| `jti` | evet | Biletin tekil kimliği (log, tek kullanım). 1–128 bayt |
| `nbf` | hayır | Bu zamandan önce geçersiz |
| `iss` | hayır | Yayıncının adı (bilgi; yayıncıyı `kid` belirler) |
| `ext` | `T`'ye bağlı | **Oyunun kendi talepleri** (`Claims<T>::game`): karakter, sınıf, ekipman, parti, bölge, MMR, DLC/hak, izleyici rolü … Yoksa `null` okunur |
| (altbilgi) `kid` | evet | Yayıncı anahtarının kimliği (`[A-Za-z0-9._-]`, ≤ 64) |

Zamanlar RFC 3339 dizgisidir (PASETO'nun kayıtlı talepleri böyle;
JavaScript'in `toISOString()` çıktısı `…T12:00:00.000Z` ve vektörlerin
`…+00:00` biçimi okunur), katı ayrıştırılır: okunamayan zaman reddedilen
bilettir, varsayılana düşen değil. Bilinmeyen başka talepler yok sayılır
(lobi kendi taleplerini taşıyabilir); yinelenen anahtar reddedilir.

## 5. Doğrulama sırası ve ret nedenleri

| # | Kontrol | Ret (`reason`) |
|---|---|---|
| 1 | Boyut (≤ 4096), `v4.public.` başlığı, base64url, `{"kid":…}` altbilgisi | `malformed` |
| 2 | `kid` → güvenilen anahtar | `unknown_key` |
| 3 | Ed25519 imza (`verify_strict`) | `signature` |
| 4 | Taleplerin şekli/tipleri (`T` dahil) | `claims` |
| 5 | `aud` | `audience` |
| 6 | `nbf`/`iat` kayma payından fazla gelecekte; `exp` kayma payını da geçti; `exp − iat` izin verilen ömürden uzun | `not_yet_valid`, `expired`, `lifetime` |
| 7 | Oyunun kendi kontrolü (`with_check`) | `game` + oyunun adı |
| 8 | Yalnız tek kullanımda: kimlik daha önce görüldü / bekçi cevap veremedi | `replayed`, `replay_unavailable` |

1–7 durumsuzdur (bilet tek başına cevaplar); 8 en son sorulur, böylece
başka sebeple reddedilen bilet **tüketilmez**. Kayma payı her iki yönde
doğru tarafta uygulanır: istemci saati geride olan geçerli bileti
reddetmez, süresi kayma payı kadar geçmiş bileti kabul etmez (testler iki
kenarı da kilitler). Varsayılanlar: kayma 30 sn, en uzun ömür 900 sn.

## 6. Yeniden kullanım politikası

**Varsayılan: süresi içinde yeniden kullanılabilir.** Motorun resume
semantiği (RECONNECT §5) yeni bağlantının AUTH'ta aynı kimlik bilgisini
sunmasına dayanır; `gsb-client`'in `Credentials`'ı da bunu söyler. Tek
kullanımlık bilet, AUTH_RESULT'ı göremeden düşen bir bağlantının
istemcisini lobiye geri gönderirdi. Kısa ömür (dakikalar) sızan biletin
penceresini zaten daraltır.

**Opt-in tek kullanım** (`Validator::single_use(ReplayGuard::spawn(n))`,
config'de `single_use = true`):

- Görülen kimlik kümesi paylaşılmalı (doğrulayıcı bağlantı başına
  worker'da koşar) ve kilit yok: kümeyi **tek küçük aktör** tutar
  (`gsb_ticket::replay`). Doğrulama `(jti, saklama sonu, şimdi)` yollar,
  oneshot'ta hükmü bekler; aktörün döngüsü yalnız kutusunu bekler. Bir
  kontrol bir hash araması + ekleme: worker'ın zaten yaptığı Ed25519
  doğrulamasının yanında darboğaz değil.
- **Sınırlı bellek:** en çok `replay_capacity` kimlik; her biri biletin
  `exp`'i + kayma payına kadar tutulur (sonrası zaten `expired`), bir
  sonraki kontrolde budanır. Dolu küme, dolu kutu (1024) ya da gitmiş
  aktör → `replay_unavailable`: **kapalı başarısız olur**, sayılır.
- **Kapsam tek süreç:** aynı `aud`'lu iki sunucu kümeyi paylaşmaz. Daha
  fazlası gerekiyorsa bilet zaten tek bir sunucunun odasını sabitler.
- Tek kullanımda yeniden bağlanma taze bilet ister (lobiden yeni izin).

## 7. Anahtar döndürme

Doğrulayıcı küçük bir güvenilen açık anahtar kümesi tutar, her biri
kendi `kid`'iyle (config: `issuer_keys = ["kid:<64 hex>"]`).

1. Yeni anahtar (`lobby-2026-11`) üretilir; açık yarısı **önce** bütün
   oyun sunucularının `issuer_keys`'ine eklenir (eskisinin yanına).
2. Lobi yeni anahtarla basmaya geçer; eski biletler süreleri dolana kadar
   eski `kid`'le doğrulanmaya devam eder.
3. En uzun bilet ömrü (+ kayma) geçince eski anahtar listeden çıkarılır;
   o `kid`'li bilet artık `unknown_key`.

Sızan yayıncı anahtarı için aynı adımlar, 3. adım hemen. Yinelenen `kid`,
boş küme, zayıf (küçük mertebeli) açık anahtar başlangıçta reddedilir.
Anahtar ayrıştırma hataları anahtarın hiçbir karakterini yazmaz;
`IssuerKey`'in `Debug`'ı gizli yarıyı göstermez, gizli yarı düşerken
silinir; `JoinGrant`'ın `Debug`'ı bileti göstermez.

## 8. Oyunun talepleri ve kontrolleri

`gsb-ticket` oyunun taleplerine göre **jeneriktir**: `Claims<T>`,
`Validator<T>`, `Issuer::mint(&Claims<T>)`; `T: Serialize +
DeserializeOwned` oyunun tipidir (örnekte `Loadout { character, class }`).
Standart talepleri crate denetler; `T`'yi yalnız tipler ve iletir.

**Oyunun durumsuz kontrolü:** `Validator::with_check(|c: &Claims<T>|
-> Result<(), GameReason>)` imza ve standart taleplerden sonra koşar.
`GameReason` oyunun ret adıdır (`const SEASON: GameReason =
GameReason::new("season_pass");` — geçersiz ad derleme hatası); ret
`ERROR 10` + `ticket rejected by the game: season_pass` olarak gider ve
**tam o adla** sayılır (§9).

**Talepler katılım yoluna kadar gider:** doğrulayıcı `ext`'i, yayıncının
imzaladığı **birebir JSON baytları** olarak `ValidatedTicket::extra`'ya
koyar. Motor onu okumaz; tanımlı katılımla (`SpawnPlayer` →
`RoomOp::Join` → `RoomControl::Resume` / `ShardMsg::Join`) taşır ve iki
kancaya `Joiner { identity, claims }` olarak verir:

- `GameLogic::on_join_verified(world, conn, &Joiner)` — oda ve shard
  aktörlerinin her taze katılımda çağırdığı (varsayılanı `on_join_as`:
  her oyun değişmeden derlenir ve davranır). Kit odaları bunu
  `Game::spawn_player_verified` / `TeamGame::spawn_team_player_verified`
  kancalarına iletir (varsayılanları `spawn_player_as` /
  `spawn_team_player_as`).
- Sharded odanın ev yönlendiricisi: `HomeShard` artık `Arc<dyn
  HomeRoute>`; her eski `(bağlantı, kimlik)` kapanışı kendiliğinden bir
  `HomeRoute`, talepleri okuyan yönlendirici `route_verified(|conn,
  joiner| …)`.

`extra` neden `Option<Bytes>`: doğrulayıcı ile oyun ayrı crate'ler, bir
biçimde anlaşırlar (JSON); `Bytes` `Send + Sync`, katılım yolunda ucuz
kopyalanır, `ValidatedTicket`'ı `Eq`/`Debug` tutar, motor biçimden
bağımsız kalır. Tip-silinmiş bir yük (`Arc<dyn Any>`) `Eq`/`Debug`'ı
kaybettirir ve süreç dışına taşınamazdı. Bedeli: oyun katılımda küçük bir
JSON'u bir kez daha çözer (katılım başına bir kez). Yerel auth yolunda ve
`extra` vermeyen doğrulayıcıda `claims` `None`'dır; boş `player` döndüren
bir doğrulayıcının oturumu anonimdir ve talep taşımaz (`gsb-ticket` boş
`sub`'ı `claims` ile reddeder).

### Hangi kontrol nerede?

| Kontrol | Nerede | Neden |
|---|---|---|
| İmza, `kid`, `aud`, zamanlar, ömür | Doğrulayıcı (`gsb-ticket`) | Bilet tek başına cevaplar; tick dışı worker |
| Oyunun durumsuz kuralı: en düşük istemci sürümü, bölge uyuşmazlığı, "bu mod sezon bileti ister", çıkmamış sınıf | Doğrulayıcı, `with_check` | Yalnız biletin taleplerine bakar; tick'i hiç beklemez, adıyla sayılır |
| Tek kullanım | Doğrulayıcı, `ReplayGuard` aktörü | Süreç çapında küçük durum; aktörde |
| Durumlu oyun kuralı: kadro, dolu takım, bu odadan atılmış oyuncu, oda kapasitesi | Odanın katılım kancası (`on_join_verified` / `spawn_player_verified`), tick'te | Dünya durumunu ister; zaten oyun mantığı |
| Talebi kullanmak: karakteri sınıfına göre yerleştirmek, ekipmanı vermek | Odanın katılım kancası | Doğrulanmış kaynaktan, istemcinin sözünden değil |
| Ev shard'ı (K4) | `HomeRoute` / `route_verified` | Registry katılım dağıtımında, saf ve senkron |

## 9. Sayım ("her şeyi saymalıyız")

Bilet-auth sunucusunun karar verdiği her AUTH tam bir kez sayılır
(`gsb_core::metrics::TicketCounts`, bağlantı aktörünün örneğinde delta):

- `gsb_net_tickets_accepted_total`
- `gsb_net_tickets_rejected_total{reason}` — kapalı küme, sıfırlar
  dahil: `missing` (bilet yok), `malformed`, `unknown_key`, `signature`,
  `claims`, `expired`, `not_yet_valid`, `lifetime`, `audience`,
  `replayed`, `replay_unavailable`, `game`, `other` (doğrulayıcının serbest
  metinli `Rejected`'ı), `timed_out` (kancanın süresi), `validator_lost`
  (doğrulayıcı worker'ı cevapsız öldü)
- `gsb_net_ticket_game_rejects_total{check}` — oyunun adları (sayılınca
  görünür); 8 adı aşan ad `reason="game"`'de ve
  `gsb_net_ticket_game_names_dropped_total`'da (sıfır değilken) sayılır
- log: `gsb-metric scope=tickets accepted=… rejected=… reject_<reason>=… game_<ad>=…`
  (bilet-auth sunucusu bir AUTH'a karar verdiyse)

Defter kapalıdır: `accepted + Σ rejected = bilet-auth sunucusunun
cevapladığı AUTH`. Her iki dışa açım (Prometheus, OTLP) aynı aileleri
taşır. Loadgen metrik teli taşımaz (düzen ve sihirli sayı değişmedi;
yük istemcileri yerel auth kullanır).

Not: bağlantının sayıları diğer deltaları gibi akar — sonraki karesinde
(500 ms aralıkla) ya da kapanışında. Reddedilip boşta bekleyen bir
bağlantının reddi kapanışına kadar raporda görünmez; durdurmanın son
raporu hepsini taşır (F35).

## 10. Katılım izni (join grant) ve istemci yardımcısı

`gsb_ticket::JoinGrant` lobinin istemciye cevabıdır (JSON):

```json
{ "doors": [ { "transport": "udp", "addr": "203.0.113.7:7777" },
             { "transport": "tcp", "addr": "203.0.113.7:7778" } ],
  "udp_server_key": "<64 hex: sunucunun statik X25519 açık anahtarı>",
  "ticket": "v4.public.…", "room": 42 }
```

`udp_server_key` RUDP-SECURITY karar 3'ün uygulamasıdır: istemci sunucu
anahtarını **izinden** sabitler, yani HTTPS'le ulaştığı lobiye güvenir,
oyun sunucusunun kendisine değil. `gsb-client`'in `grant` feature'ı:
`grant::connect(&grant, Transport::Udp, &[])` kapıyı açar (udp: izindeki
anahtarla mühürlü el sıkışma; izinde anahtar yoksa reddeder — izin
istemciyi asla düz metin kapıya göndermez; tls/quic: çağıranın kök
sertifikaları ve kapının host adı), `grant::join(..)` biletle AUTH +
izindeki odaya JOIN yapar.

## 11. Sunucu config'i: `[ticket]` (feature `ticket`)

```toml
[ticket]
issuer_keys = ["lobby-2026-10:<64 hex>"]
audience = "eu-1"
max_skew_secs = 30
max_lifetime_secs = 900
single_use = false
replay_capacity = 100000
timeout_ms = 2000
```

`otlp` gibi: tablo her derlemede ayrışır (`Config::ticket`), `ticket`
feature'ı olmayan derleme onu adıyla reddeder (`TicketNotBuilt`).
Bilinmeyen anahtar, hatalı değer reddedilir (`BadTicket`; iletideki 16+
hex dizileri gizlenir); tablo ile çağıranın kendi kancası birlikte
verilirse başlatma durur (`TicketHookConflict`: sunucu başına tek
otorite). Config'den kurulan doğrulayıcı oyunun taleplerini tiplemez
(`ext` doğrulanmış bayt olarak geçer) ve oyun kontrolü koşturmaz: tipli
kontrol isteyen oyun doğrulayıcıyı kodda kurar ve `ServerHooks::ticket`
olarak verir (örnek böyle yapar).

## 12. TypeScript/Nuxt lobisi bilet basar (ÖRNEK — burada test edilmedi)

Node'un yerleşik Ed25519'u yeter; PASETO v4.public 20 satır. (Bir PASETO
kütüphanesi de kullanılabilir — ör. npm'de `paseto-ts`; API'si sürüme
göre değişir, burada doğrulanmadı.)

```ts
// server/utils/gsbTicket.ts — ÖRNEK, gsb deposunda test edilmez.
import { createPrivateKey, randomBytes, sign } from 'node:crypto'

const HEADER = 'v4.public.'
// PKCS#8 sarmalı: 32 baytlık Ed25519 tohumunun önüne sabit 16 bayt.
const PKCS8_PREFIX = Buffer.from('302e020100300506032b657004220420', 'hex')

const le64 = (n: number) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b }
const pae = (...p: Buffer[]) => Buffer.concat([le64(p.length), ...p.flatMap((x) => [le64(x.length), x])])

export function mintTicket(seedHex: string, kid: string, claims: object): string {
  const key = createPrivateKey({
    key: Buffer.concat([PKCS8_PREFIX, Buffer.from(seedHex, 'hex')]),
    format: 'der',
    type: 'pkcs8',
  })
  const m = Buffer.from(JSON.stringify(claims))
  const f = Buffer.from(JSON.stringify({ kid }))
  const sig = sign(null, pae(Buffer.from(HEADER), m, f, Buffer.alloc(0)), key)
  return HEADER + Buffer.concat([m, sig]).toString('base64url') + '.' + f.toString('base64url')
}

// server/api/join.post.ts — oturumu açık kullanıcıya izin.
export default defineEventHandler(async (event) => {
  const user = await requireUserSession(event) // lobinin kendi oturumu
  const now = Date.now()
  const ticket = mintTicket(process.env.GSB_ISSUER_SEED!, 'lobby-2026-10', {
    sub: user.id, room: 42, aud: 'eu-1',
    iat: new Date(now).toISOString(), exp: new Date(now + 120_000).toISOString(),
    jti: randomBytes(16).toString('hex'),
    ext: { character: user.characterId, class: user.class },
  })
  return {
    doors: [{ transport: 'udp', addr: '203.0.113.7:7777' }],
    udp_server_key: process.env.GSB_UDP_PUBLIC_KEY, // sunucunun açılış logu / ServerHandle::udp_public_key
    ticket, room: 42,
  }
})
```

Tohum (`seedHex`) lobinin sırrıdır; açık yarısı `kid:hex` olarak oyun
sunucularının `issuer_keys`'ine girer (`IssuerKey::trusted()` /
Node'da `createPublicKey(key).export({format:'der',type:'spki'})`'in son
32 baytı). `room` JS'te güvenli tamsayı sınırında kalmalı (2^53).

## 13. B22 tarifi: oyun kendi servisini çağırır

B22 kararı (2026-10-03): HTTP/NATS/Redis adaptörü **oyunun işi**; motor
iki dikiş verir. Yeni bağımlılık yok — adaptörün istemcisi (HTTP için
`reqwest`/`hyper`, NATS için `async-nats`, Redis için `redis`) oyunun
crate'inin seçimidir.

**İstek → dış servis: `RequestDecision::External`.** Odanın
`handle_request`'i (kit'te `Game::handle_request`) sahiplenen bir future
döndürür; oda isteği bekleyen olarak kaydeder, cevap sonraki bir tick'te
gelir, süre sınırı odanın istek zaman aşımıdır (RPC-CONTROL-PLANE §1–3).
Adaptörü bir **aktör** yapın (kilit yok, tek bekleme): sınırlı bir kutu,
istemci bağlantısını sahiplenen tek görev, oneshot ile cevap — demo'nun
`EconomyService`'inin şekli.

```rust
/// The game's adapter: one task owns the client; requests come in over
/// a bounded channel, each with its own reply.
#[derive(Clone)]
pub struct Inventory { tx: tokio::sync::mpsc::Sender<(String, tokio::sync::oneshot::Sender<Result<Bytes, String>>)> }

impl Inventory {
    pub fn spawn(/* the game's HTTP / NATS / Redis client */) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move {
            while let Some((item, reply)) = rx.recv().await {
                let answer = call_the_service(&item).await; // the game's own client
                let _ = reply.send(answer);
            }
        });
        Self { tx }
    }

    pub fn grant(&self, item: String) -> impl Future<Output = Result<Bytes, String>> + Send + 'static {
        let tx = self.tx.clone();
        async move {
            let (reply, answer) = tokio::sync::oneshot::channel();
            tx.try_send((item, reply)).map_err(|_| "inventory busy".to_string())?;
            answer.await.map_err(|_| "inventory gone".to_string())?
        }
    }
}

// In the game's request hook:
fn handle_request(&mut self, _w: &mut World, _c: &TickCtx, req: &RpcRequest,
                  _p: &HashMap<PlayerId, Entity>) -> Option<RequestDecision> {
    let item = String::from_utf8_lossy(&req.payload).into_owned();
    Some(RequestDecision::External(Box::pin(self.inventory.grant(item))))
}
```

Dolu kutu (`try_send`) normal bir ret olarak döner ve sayılır; servisin
hatası `Err(String)` → istemciye ret; süre aşımı odanın süpürgesinde
sayılır (`requests_timed_out`). Sonuç tick'e asla beklenmez.

**Maç sonucu → dış dünya: `ServerHandle::match_results`.** Oda
kapanırken `GameLogic::match_result` yükü bu sınırlı kutuya düşer;
kompozisyon kökü onu okuyup kendi servisine yayımlar:

```rust
let mut handle = gsb_server::start_game_server_with(module, cfg, hooks, None).await?;
// Move the receiver out (a closed stand-in stays in the handle).
let (_, closed) = tokio::sync::mpsc::channel(1);
let mut results = std::mem::replace(&mut handle.match_results, closed);
tokio::spawn(async move {
    while let Some(r) = results.recv().await {
        // r.room, r.payload: publish to the game's own NATS subject /
        // Redis stream / HTTP endpoint, with its own retry policy.
        publish(r.room.0, r.payload).await;
    }
});
```

Yayımlama başarısızsa yeniden deneme politikası oyunundur; kutu dolarsa
oda sonucu düşürür ve sayar (`match_results_dropped_{full,closed}`).

## 14. Örnek: `examples/lobby`

`cargo run -p gsb-example-lobby` — tek süreçte lobi, mühürlü rUDP + TCP
kapılı sunucu, istemciler (README). Oyun 2B demo'yu sarar: her karakter
biletin imzalı `ext`'indeki sınıfın tarafında doğar (büyücü x = −30,
savaşçı x = +30); çıkmamış sınıf oyunun kontrolüne takılır
(`unknown_class`). Testi (`tests/lobby.rs`) akışı, ret kodlarını ve
sayaçları kilitler; ikinci test sunucuyu `[ticket]` tablosundan kurar.

## 15. Testler

| Sözleşme | Test |
|---|---|
| Resmi PASETO v4 vektörleri (imza + doğrulama), başka sürüm/amaç reddi, PAE | `gsb-ticket/src/paseto/tests.rs` |
| Her doğrulama kuralı tek ret; kayma/sona erme/ömür kenarlarının iki yanı; anahtar döndürme; oyunun kontrolü adıyla; tek kullanım ve tüketilmeyen ret; motor kancasının şekli | `gsb-ticket/src/validator/tests.rs` |
| Bekçi: bir kez kabul, süre sonunda unutma, dolu küme kapalı başarısız | `gsb-ticket/src/replay.rs` |
| RFC 3339 okuma/yazma | `gsb-ticket/src/time.rs` |
| `Debug` gizli anahtarı göstermez; ayrıştırma hatası anahtarı yazmaz; zayıf anahtar reddi | `gsb-ticket/src/keys.rs` |
| Config tablosu; bilinmeyen anahtar; anahtar yazılmaz | `gsb-ticket/src/config.rs`, `gsb-server/src/boot/start/ticket/tests.rs` |
| Her AUTH kararı tek sebeple sayılır; oyun adı ayrı; delta iki kez sayılmaz | `gsb-core/tests/conn_counts/tickets.rs` |
| Talepler yönlendiriciye ve ev shard'ının kancasına, tek odada resume düşüşünden kancaya ulaşır; yerel auth taşımaz | `gsb-core/tests/join_identity/claims.rs` |
| Her kit odası talepleri oyunun spawn'ına verir | `gsb-kit/src/game/tests/claims.rs` |
| Uçtan uca: izin → mühürlü rUDP/TCP → sınıfa göre doğuş; sahte/eski/oyunca reddedilen bilet; sayaçlar; `[ticket]` tablosundan sunucu | `examples/lobby/tests/lobby.rs` |

## 16. Yapılmayanlar

- Tek kullanım kümesi süreçler arası paylaşılmıyor (§6).
- Config'den kurulan doğrulayıcıda oyun kontrolü yok (`GameModule`'den
  bir kontrol kancası istenirse eklenebilir; bugün kodla kurulur).
- PASERK (`k4.public.` anahtar dizgileri, `k4.pid` kimlikleri) yok; `kid`
  serbest dizgi, anahtar hex.
- Örtük iddia (implicit assertion) kullanılmıyor (boş); `aud` aynı işi
  görüyor ve JS kütüphanelerinde daha taşınabilir.
