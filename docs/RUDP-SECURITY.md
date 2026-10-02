# gsb: rUDP Güvenliği — Noise NK el sıkışması + kendi kayıt katmanımız

> **Durum:** TASARIM + ÇEKİRDEK (x1 turu, 2026-10-02).
> - **Bu turda yapılan:** kripto çekirdeği `gsb_net::seal` (sans-IO, soket
>   yok, tokio yok) ve bu doküman. Testli, ama **rUDP'ye bağlı değil**.
> - **Bağlama:** B5a (§12). O tura kadar rUDP bugünkü gibi düz metindir
>   (BACKLOG B5).
> - **B3 (2026-10-02):** kriptosuz CID ve göç yapıldı, opt-in
>   (`udp_migration`); kind bayt haritası kesinleşti (§5, B109). Ne
>   yapıldığı ve B5a'ya ne kaldığı: §7.
>
> Kaynak araştırma: x1 araştırma raporu (2026-10-02). Bu doküman onun
> §4–§7'sini ve maintainer'ın 10 kararını sözleşmeye çevirir.

## 1. Karar: DTLS kütüphanesi değil, Noise NK + kendi kayıt katmanımız

**Seçilen yol (D):** WireGuard/QUIC biçimli bir düzen.
- **El sıkışma:** Noise `NK_25519_ChaChaPoly_BLAKE2s` (`snow` 0.10, yalnız
  RustCrypto). Bugünkü durumsuz çerez el sıkışmasının proof/accept adımına
  biner: **0 ek RTT**.
- **Kayıt:** `[kind][CID][sayaç]` başlığı + ChaCha20-Poly1305. Başlık
  AEAD'nin ilişkili verisidir (AAD).
- **Göç:** RFC 9146 §6 kuralı: kimliği doğrulanmış + daha yeni + yolu
  doğrulanmış.

**Neden DTLS değil (araştırma §2–§3, doğrulanmış):**

| | DTLS (`dimpl` 0.7.4, en iyi aday) | Noise NK + kendi kayıt |
|---|---|---|
| Saf Rust | `rust-crypto` özelliğiyle evet; varsayılanı aws-lc (C) | Evet (`cargo tree` kanıtı §13) |
| CID (RFC 9146/9147) | **Yok** — göç (B3) için upstream'e yazılmalı | Bizim tasarım (§7) |
| Alma/gönderme bölünmesi | Tek `Dtls` nesnesi ikisini de yapar → demux ile writer arasında paylaşım (kilit ya da mesaj) | `Sealer` / `Opener` ayrı sahipli, paylaşım yok |
| El sıkışma RTT | Çerez + 1 RTT; AUTH'a kadar 3 RTT (bugün 2) | 0 ek RTT |
| Ek yük | DTLS 1.3 + CID ≈ 28 B | c→s 33 B, s→c 25 B |
| C#/Unity | BouncyCastle C#'ta yalnız DTLS 1.2 | BouncyCastle C#'ta X25519 + ChaCha20-Poly1305 + BLAKE2s var → küçük port |
| Bedel | — | Standart uyum yok; kayıt katmanını biz yazıyoruz ve inceletiyoruz (§11) |

**Elenenler:**
- `rtc-dtls`: `ring` zorunlu, yalnız 1.2, CID yok.
- `webrtc-dtls`: async, eski hat.
- `rustls`: DTLS yok.
- `noxtls`: GPL.
- OpenSSL/wolfSSL/mbedTLS: C.

**Fikir kaynakları:**
- WireGuard: kayıt düzeni, ~2000'lik replay penceresi.
- QUIC (RFC 9000/9001): anahtar fazı, bütünlük sınırı, stateless reset,
  yol doğrulaması.
- netcode 1.02: sunucu yeniden başlarken nonce tekrarı tuzağı. Bizde
  yapısal olarak yok: her oturum efemeral DH ile yeni anahtar alır.

## 2. Maintainer kararları (2026-10-02)

| # | Soru | Karar |
|---|---|---|
| 1 | `ring` / C kabul mü? | **Hayır**, bu yolda `ring`, `aws-lc` ve C yok. RustCrypto'nun içerideki SIMD `unsafe`'i kabul (bizim kodumuz değil); bizim kod `unsafe_code = "forbid"` altında |
| 2 | DTLS uyumu gerekli mi? | Hayır. Yalnız kendi istemcilerimiz: Rust + ileride C#/Unity portu |
| 3 | Sunucu kimliği | **Statik X25519 anahtarı.** Platform açık anahtarı bilet ve adresle birlikte verir, istemci sabitler (pin). PKI/sertifika yok |
| 4 | netcode tarzı bilet anahtarı kipi | Şimdi yok |
| 5 | Kriptosuz göç (B3) | **Opt-in.** Kripto gelince varsayılan açık |
| 6 | Kripto gelince düz metin | **Mühürlü (sealed) üretim varsayılanı;** düz metin yalnız açık bir dev/LAN anahtarıyla |
| 7 | CID rotasyonu, sayaç gizleme | Sonra (§10) |
| 8 | Dış güvenlik incelemesi | Planlı: kayıt katmanı bağlanınca (§11) |
| 9 | Yeniden başlatmadan sağ çıkan stateless reset anahtarı config'de | Evet (§8) |
| 10 | Göçte yeni adrese erken gönderim mi? | Hayır: **yeni yol doğrulanana kadar eski yolda beklenir** (§7) |

## 3. Tehdit modeli

**Saldırgan türleri:**
- **Yol dışı:** paketleri göremez, adres sahteler.
- **Koklayıcı:** paketleri görür, enjekte eder.
- **Yol üstü:** düşürür, geciktirir, değiştirir.

| Saldırı | Bugün (düz metin) | B3 (kriptosuz CID, opt-in) | Kripto (B5a sonrası) |
|---|---|---|---|
| Kare okuma | Koklayıcı her şeyi okur | Aynı | Şifreli. Görünen: CID (c→s), sayaç, boy, zamanlama |
| Kare enjeksiyonu / değiştirme | Koklayıcı enjekte eder; yol dışı, portu bilirse sahte RAW/FRAG basar | Aynı | AEAD reddeder → `seal_forged` |
| Tekrar oynatma | Mümkün | Mümkün | Replay penceresi → `seal_replayed` / `seal_too_old` |
| Oturum kaçırma | Adres sahteciliğiyle kısmen | **CID taşıyıcı jetondur:** CID'yi koklayan, düz metin PATH_CHALLENGE'ı kendi adresinden yanıtlayıp s→c akışını kendine çeker | Challenge şifreli, yanıtlanamaz; eski yolda beklenir |
| Yansıtma / amplifikasyon | Çerez + oran ≤ 1 | + 3x bütçe + yol doğrulaması | Aynı. Accept (77 B) proof'tan (~67 B) büyük, ama yalnız çerezle kanıtlanmış adrese gider |
| DH seli (CPU) | — | — | DH yalnız çerez doğrulandıktan **sonra**; B89 kaynak başına + global bütçe |
| Sahte sunucu | Mümkün | Mümkün | İstemci sunucu açık anahtarını sabitler; NK msg2'yi yalnız gerçek sunucu üretebilir |
| Sunucu yeniden başlarsa | İstemci 5 sn REL sınırını bekler | Aynı | Stateless reset jetonuyla hemen biter (B5b) |
| Pasif bağlanabilirlik (gizlilik) | Adres | CID ağlar arası sabit | Aynı. Çözüm CID rotasyonu (§10, sonra) |

**Kapsam dışı:**
- Uç noktaların ele geçirilmesi.
- Sunucu statik anahtarının sızması. Geçmiş oturumlar efemeral DH
  sayesinde korunur (ileri gizlilik), ama sızan anahtarla sunucu taklit
  edilebilir. Çözüm: platform yeni açık anahtarı dağıtır.
- Trafik analizi.

## 4. El sıkışma akışı (bugünkü çerez el sıkışmasının üstünde, 0 ek RTT)

```text
istemci                                          sunucu
HELLO{n,0}                          18 B  ──▶
                                          ◀──  HELLO{n,cookie}   18 B   durum yok, DH yok
proof{n,cookie} + caps + msg1       ~67 B ──▶  1) Msg1::parse   (yalnız uzunluk)
  msg1 = e(32) + şifreli yük + tag(16)          2) çerez doğrulaması (bugünkü kod)
                                                3) Msg1::cookie_verified → DH burada
                                          ◀──  ACK{1} + msg2     5 + 72 B
                                                 msg2 = e(32) + {cid u64, reset jetonu 16} + tag(16)
SEALED REL AUTH(bilet)              ──▶        AUTH artık şifreli kanalın içinde
```

**Kurallar:**
- **DH sırası API'de açık.** `Msg1::parse` yalnız boy kontrol eder
  (48..=80 B). X25519 işlemleri yalnız `Msg1::cookie_verified` içinde
  koşar. Çağıran oraya ancak çerez kontrolünden sonra gelir.
  - Sunucu el sıkışma başına 3 skaler çarpım yapar: `es`, efemeral üretimi,
    `ee`. snow statik açık anahtarı her responder kurulumunda yeniden
    türettiği için +1 eder.
  - Bunlar B89 bütçesinin konusu ve B5a'da ölçülecek.
- **Prologue = `"gsb-rudp-seal/1\0"` + bağlam.** B5a bağlam olarak HELLO
  nonce'u ve çerezi verir. Böylece Noise dökümü o çerez alışverişine
  bağlanır: başka bir çerezle yakalanmış msg1 tutmaz. Bağlam uyuşmazlığı
  `HandshakeError::Decrypt` olur.
- **msg1 yükü:** en çok 32 B (`MSG1_PAYLOAD_MAX`), ör. yetenek bitleri.
  - NK msg1 yükü sunucu anahtarına şifrelidir ama **tekrar oynatılabilir**
    ve ileri gizliliği yoktur.
  - Bu yüzden **bilet asla msg1'de gitmez.** Bilet, el sıkışmadan sonra
    SEALED REL içinde AUTH olarak gider.
- **msg2 yükü sabit 24 B (`Accept`):** tel CID'si (rastgele, sunucu seçer;
  `gsb_core::ConnectionId` değil) + stateless reset jetonu.
- **İdempotent proof:** sunucu oturum başına msg2 baytlarını saklar. Aynı
  proof yeniden gelirse aynı baytları yeniden gönderir; ikinci DH yok.
  İstemci proof'u yeniden gönderirken `Initiator::msg1()`'in aynı
  baytlarını kullanır.
- **Sahte accept el sıkışmayı öldürmez.** `Initiator::finish` başarısız
  olursa snow durumu geri alınır; istemci gerçek msg2'yi beklemeye devam
  eder (testli).
- **İstemci kimliği:** NK katmanında istemci anonimdir; kimliğini şifreli
  AUTH biletiyle kanıtlar. `Session::handshake_hash()` kanal bağlama için
  açıktır: platform bileti ileride bu hash'e bağlayabilir (opsiyonel).
- **Sunucu statik anahtarı config'de** (B5a). Yoksa ya da bozuksa
  başlatma hatası olur; sessiz düz metin geri düşüşü yoktur (SECURITY §2
  karar 3'ün ilkesi).
- **Hata adları (`HandshakeError`):** `Malformed` (DH'den önce), `Decrypt`,
  `PayloadTooLarge`, `Internal`. Her biri ayrı sayılır.

**Neden BLAKE2s (SHA-256 değil):**
- WireGuard'ın seçimi.
- SHA uzantısı olmayan 32/64 bit CPU'larda (mobil, Unity) yazılımda hızlı.
- snow çözücüsünde `ring`'siz var.
- C# tarafında BouncyCastle'da var.
- Reset jetonu da aynı hash'i kullanır: protokolde tek hash fonksiyonu.

## 5. SEALED kayıt düzeni

```text
c→s  [kind 1][cid u64 LE 8][sayaç u64 LE 8][şifreli iç datagram][tag 16]   ek yük 33 B
s→c  [kind 1]              [sayaç u64 LE 8][şifreli iç datagram][tag 16]   ek yük 25 B
kind = 0x40 | anahtar fazı biti (0x01)
```

- **İç datagram, bugünkü datagramın aynısıdır** (RAW/REL/ACK/FRAG/PROBE/
  REPORT ve B3'ün PATH_*'ı). Anlamları içeride değişmez.
  - B5a bütçeyi ek yük kadar küçültür: 1472 − 33 c→s, 1472 − 25 s→c.
- **kind 0x40 — kesin (B3, B109 kapandı).** Kind bayt haritası:

  | Aralık | Anlamı |
  |---|---|
  | `0x00..=0x3F` | Düz metin türler: `0` RAW, `1` REL, `2` ACK, `3` HELLO, `4` FRAG, `5` PROBE, `6` REPORT, `7` PATH_CHALLENGE, `8` PATH_RESPONSE; `9..=0x3F` boş |
  | `0x40..=0x7F` | SEALED kayıt: `0x40 \| faz` (`0x40`, `0x41`); `0x42..=0x7F` boş |
  | `0x80..=0xBF` | CID etiketli düz metin tür `0x80 \| k` (yalnız c→s, B3): `[0x80\|k][u64 cid][k'nın gövdesi]` |
  | `0xC0..=0xFF` | Boş (SEALED etiket bitini almaz: CID'yi kendi başlığında taşır) |

  - Etiketli düz metin ile SEALED c→s, CID'yi **aynı konumda** taşır
    (bayt 1..9): demux iki biçimde de oturumu aynı yerden bulur.
  - Ayrıklık ve konum eşitliği derleme zamanı `assert`'leridir
    (`crates/gsb-net/src/udp/mod.rs`).
  - Eski sunucu bilmediği türü düşürür ve `udp_datagrams_malformed`'a
    sayar (`_ => bad_datagrams` kolu, oturuma dokunmaz); yeni istemci
    zaten yalnız CID verilmişse etiketler.
- **AAD = başlığın kendisi.** `Sealer::seal` başlığı kendisi yazar ve AAD
  olarak kullanır; başlık ile nonce ayrışamaz.
- **Nonce:** Noise ChaChaPoly kodlaması: 4 sıfır bayt + sayaç (u64 LE).
  Yön başına ayrı anahtar: Noise `Split()`; ilk anahtar istemci → sunucu.
- **Sayaç sınırı `SEAL_LIMIT` = 2^62:** QUIC paket numarası tavanı. Noise
  2^64−1'i REKEY'e ayırır. Saniyede 1M datagram ile ~146 bin yıl.
  - Önemli olan: sarma yok, sert ret var (`SealError::CounterExhausted`).
  - Açıcı bu sınırı aşan sayacı AEAD'ye hiç sokmaz (`seal_malformed`).
- **Replay penceresi `REPLAY_WINDOW` = 1024** (RFC 6479 biçimli bit halkası).
  - Oturum başına 128 B; 100k oturumda 12,8 MB. WireGuard'ın ~2048'i 25,6 MB
    olurdu.
  - Oyun oturumu saniyede onlarca–birkaç yüz datagram gönderir. 1024 sayaç
    saniyelerce sırasızlık demektir; daha geç gelen oyun için zaten bayattır
    ve REL bandı onu yeniden gönderir.
  - Pencere içinde sırasız gelen kabul edilir; tekrar ve çok eski reddedilir.
  - Pencere **yalnız doğrulanmış** datagramla ilerler: sahteci gelmemiş
    yuvaları yakamaz (mutasyonla doğrulandı).
- **Bütünlük sınırı `INTEGRITY_LIMIT` = 2^36** (RFC 9001 §6.6,
  AEAD_CHACHA20_POLY1305). Oturum ömrü boyunca, tüm anahtarlar üzerinden
  sayılır. Aşılınca her açma `seal_integrity_limit` ile reddedilir ve
  oturum kapatılmalıdır.

**Açıcının retleri ("her kaybı say").** Sıra en ucuzdan pahalıya; ilk
tutan ad datagramın adıdır, tek datagram tek ad alır:

| Sıra | `Refusal` | Sayaç adı | Anlamı |
|---|---|---|---|
| 1 | `IntegrityLimit` | `seal_integrity_limit` | Oturum 2^36 sahte sınırını aştı; hiçbir şey açılmaz |
| 2 | `Malformed` | `seal_malformed` | SEALED değil, başlık + tag'e yetmez, ya da sayaç ≥ 2^62 |
| 3 | `TooOld` | `seal_too_old` | Pencerenin altında (gerçek olabilir) |
| 4 | `Replayed` | `seal_replayed` | Bu sayaç zaten açıldı |
| 5 | `WrongPhase` | `seal_wrong_phase` | Faz biti sayaçla çelişiyor: mevcut fazda açılmış bir sayacın altında yeni faz. Dürüst eş göndermez; AEAD'ye girmez |
| 6 | `Forged` | `seal_forged` | Kimlik doğrulama başarısız (sahte, bozuk, yanlış anahtar) |

- `Opened { plaintext, counter, newest }`: `newest` = şimdiye kadarki en
  yüksek sayaç. Göç kuralının ikinci koşuludur (§7).
- 3–5 AEAD'den önce elenir. Bu yüzden sayaçtaki tekrarı taşıyan sahte
  datagram `Forged` değil `Replayed` sayılır. Bilinçli: ucuz kontrol önce.

## 6. Anahtar fazı ve yeniden anahtarlama

**Sonraki anahtar:** Noise `REKEY(k) = ENCRYPT(k, 2^64−1, "", sıfır(32))[..32]`.
- Tek ChaCha20 bloğu, yeni bağımlılık yok.
- Tek yönlü: eski anahtar yenisinden hesaplanamaz.
- snow'un `rekey_*`'ıyla bayt bayt aynı (test). C# portu aynı vektörlerle
  sınanır.
- HKDF yerine bu seçildi: ek crate gerekmez ve Noise spesifikasyonunun
  tanımlı işlemi.

**Faz biti:** kind'in 0x01 biti = anahtar neslinin düşük biti. Sayaç
nesiller boyunca sürer (QUIC paket numarası gibi). Böylece tek bir replay
penceresi tüm fazları kapsar ve nonce hiçbir anahtar altında tekrar etmez.

**Gönderen kuralları (`Sealer::rekey`):**
1. `RekeyTooSoon`: önceki rekey'den bu yana en az `REKEY_MIN_DISTANCE`
   (= pencere, 1024) datagram mühürlenmiş olmalı.
2. `RekeyUnconfirmed`: eş mevcut fazdan bir datagramı onaylamış olmalı
   (`note_peer_ack`; RFC 9001 §6.1'in kuralı).
   - REL katmanı ACK'i gönderdiği sayaca eşler: B5a'nın işi.

**Alıcı (`Opener`):**
- Mevcut anahtar, önceden hesaplanmış sonraki anahtar ve **bir** önceki
  anahtarı tutar.
- Faz biti çevrilmiş ve sayaç şimdiye kadarkilerin hepsinden büyükse
  sonraki anahtar denenir; doğrulanırsa terfi eder.
- Faz biti eski ve sayaç yeni fazın başlangıcının altındaysa önceki
  anahtar kullanılır (sırasız gelenlere tolerans, "grace").
- Önceki anahtar, pencere faz sınırını geçince (`top ≥ başlangıç + 1023`)
  bırakılır. O andan sonra eski fazın her sayacı zaten `TooOld`'dur.
- İki gönderen kuralı birlikte şunu garanti eder: dürüst bir eşin
  datagramı, alıcının elinde anahtarı olmayan bir nesilden asla gelmez.
  Seeded model testi bunu 3 tohum × 20k adımda doğrular.

**Rekey ne zaman yapılır** (zaman/sayı politikası) B5b'nin kararıdır.
ChaCha20-Poly1305'in pratik gizlilik sınırı yoktur. Rekey ileri gizlilik
penceresini daraltmak içindir, zorunluluk değil.

## 7. CID ve göç kuralları (B3 + B5a)

**CID kuralları:**
- **Her c→s datagramı CID taşır.** İstemci kendi NAT yeniden bağlanmasını
  göremez.
- CID 64 bit, rastgeledir (`getrandom`) ve sunucu seçer. Kriptodan önce
  B3 onu accept'te düz verir; kriptodan sonra msg2'nin şifreli yükünde
  verilir.

**Adres güncelleme kuralı (RFC 9146 §6 biçimi).** Sunucu oturumun
adresini yalnız şu üçü birden doğruysa değiştirir:
1. **Kimliği doğrulanmış:** datagram `Opener::open`'dan `Ok` döndü.
2. **Daha yeni:** `Opened::newest`, yani sayaç şimdiye kadarki en yüksek.
   Eski bir datagramı başka adresten yeniden oynatmak göç tetiklemez.
3. **Yolu doğrulanmış:** yeni adrese şifreli `PATH_CHALLENGE` (8 B
   rastgele) gider; aynı değeri taşıyan şifreli `PATH_RESPONSE` o
   adresten döner. Kripto sonrası PATH_* iç kind'lerdir.

**Bekleme ve bütçe:**
- **Karar 10:** doğrulanana kadar s→c akışı **eski adreste kalır.**
  Bedeli ~1 RTT oyun bandı kaybıdır; REL zaten yeniden gönderir.
- Doğrulanmamış adrese en çok 3x bayt gider (yalnız challenge).
- RFC 9853'ün "enhanced" kipiyle eski adres de yoklanır. Taze datagramı
  başka adresle yarıştıran saldırgan challenge'ı çözemez.

**Kriptosuz göç (B3):**
- **Opt-in** (karar 5). Çünkü orada CID taşıyıcı jetondur (§3).
- Kripto gelince (B5a) varsayılan açılır.

**B3'te yapılan (2026-10-02; ayrıntı ve gerekçeler DESIGN §6
"Bağlantı göçü", kod `crates/gsb-net/src/udp/path.rs`):**
- **Config:** `udp_migration = true|false`, varsayılan `false` (kapı
  bayt bayt eskisi — testle kilitli).
- **CID:** 64 bit, `getrandom` 0.4.3, oturum başına; entropi
  başarısızsa oturum CID'siz kurulur ve sayılır, zayıf değer yok.
- **Tel (eklemeli):**
  - proof `[3][nonce][cookie][0][u8 caps]` (19 B; caps bit 0 = CID);
  - accept `[2][u32 1][u64 cid]` (13 B; yeniden proof'a aynı CID);
  - etiketli c→s `[k|0x80][u64 cid][gövde]`;
  - `PATH_CHALLENGE [7][u64 nonce]` (s→c, 9 B);
  - `PATH_RESPONSE [0x88][u64 cid][u64 nonce]` (c→s, 17 B).
- **Kurallar:** yukarıdaki üç koşulun kriptosuz karşılığı yalnız 3.'sü
  (yol doğrulaması) — 1. (kimlik) ve 2. (`newest`) B5a'nın. Doğrulanana
  kadar s→c eski yolda (karar 10); doğrulanmamış adresten GELEN girdi
  oturuma kabul edilir (RFC 9000 §9; kriptosuz bu, eski adresi
  sahtelemekten fazlasını açmaz); challenge en çok 200 ms'de bir ve
  yalnız aday konuştukça, ≤ 3× bütçe; doğrulama 3 sn'de zaman aşımına
  uğrar; üçüncü adres bekleyeni değiştirir (en yeni aday — B5a'da
  `newest` ile gerçek sıra); başka oturumun adresi aday olamaz.
- **Yol tahmini:** yeni IP'de RTT, oyun bandı tahmini ve tıkanıklık
  sıfırlanır (RFC 9000 §9.4); yalnız port değişimi korur.
- **Sayaçlar:** `udp_cids_assigned`, `udp_entropy_draws_failed`,
  `udp_cid_unknown`, `udp_path_validations_{started,timed_out,
  superseded,open_at_end}`, `udp_path_challenges_{sent,send_failed}`,
  `udp_path_amplification_capped`, `udp_path_address_in_use`,
  `udp_path_responses_unmatched`, `udp_path_changes_not_forwarded`,
  `udp_migrations`, `udp_migrations_port_only` (OPS §3).
- **Amplifikasyon notu:** bugünkü boylarla 3× sınırı bağlamaz (aday
  ≥ 9 B'lik datagram'la doğar, challenge 9 B, yeniden gönderim yalnız
  adaydan yeni datagram'la); kural ve sayacı B5a'nın mühürlü boyları
  için yerinde ve testli.

**B5a'nın B3'ten devraldığı:**
- CID'yi accept'in düz metin uzantısı yerine msg2'nin şifreli yüküne
  taşı (`Accept`); caps baytı msg1'den önce kalır.
- Etiketli düz metin yerine SEALED c→s (CID aynı bayt 1..9'da);
  PATH_* şifreli iç tür olur; demux'ın yönlendirmesi değişmez.
- Göç kuralına 1. ve 2. koşulu ekle (`Opener::open` Ok + `newest`):
  "en yeni aday" kuralı numarasızdan sayaç sırasına geçer.
- `udp_migration` varsayılanını aç (karar 5).
- B89 önce gelir: kaynak başına sayımın göçte taşınması (bugün
  registry'nin D12 sayımı ve aktörün `peer`'i ilk adreste kalır).

## 8. Stateless reset

**Anahtar:**
- Sunucu config'inde 32 B'lik reset anahtarı (karar 9). Yeniden
  başlatmadan sağ çıkar. Yoksa ve kripto açıksa başlatma hatası (B5b).
- Jeton: `HMAC-BLAKE2s(anahtar, "gsb-rudp-reset/1" ‖ cid_le)[..16]`
  (`ResetKey::token`). Aynı anahtar ve CID her açılışta aynı jetonu verir.
- Jeton accept'te (msg2) şifreli gider; yalnız istemci ve sunucu bilir.

**Akış (B5b'nin bağlayacağı biçim, QUIC RFC 9000 §10.3):**
1. Yeniden başlayan sunucu, bilinmeyen CID taşıyan c→s SEALED datagramına
   s→c bir reset yollar. Biçim: SEALED biçimli
   `[0x40|rastgele faz][rastgele baytlar][jeton 16]`.
   - Tetikleyen datagramdan **kısa** olur: amplifikasyon yok, iki uç
     arasında reset döngüsü yok.
   - En az `OVERHEAD_S2C + 1` B olur, normal SEALED datagramdan ayırt
     edilmesin diye.
   - Kaynak başına oranlanır.
2. İstemci açamadığı (`Forged`) bir datagramın son 16 baytını sabit
   zamanlı karşılaştırır (`ResetToken::matches`). Tutarsa oturum hemen
   biter. 5 sn REL sınırı beklenmez ve B7'nin resume yolu açılır.

**Açık soru (B5b):** reset datagramı önce `seal_forged`'a mı sayılır,
yoksa yalnız `seal_stateless_reset`'e mi? Öneri: yalnız ikincisi (tek
ad); `Opener` buna bir `Forged` sonrası kanca verir.

## 9. Düz metin kipi

Kripto bağlandıktan sonra (B5a) **mühürlü kip üretim varsayılanıdır**
(karar 6). Düz metin rUDP yalnız açık bir config anahtarıyla açılır:
- Örnek: `udp_plaintext = true`, "dev/LAN" diye belgelenir.
- Açılırsa başlangıçta tek bir `warn` yazılır.
- Sunucu anahtarı yoksa sessiz düz metne düşülmez; başlatma hatası olur.

## 10. Sonraya kalanlar (karar 7, 4)

- **CID rotasyonu:** ağlar arası bağlanabilirliği keser. Sunucu
  NEW_CONNECTION_ID benzeri şifreli bir iç kind ile yedek CID'ler verir;
  istemci göçte yenisine geçer.
- **Sayaç gizleme:** QUIC başlık koruması gibi, sayacı şifreli örnekten
  türetilen maskeyle örter. Bugün sayaç düz görünür: gözlemciye paket
  hızını ve kaybı söyler.
- **Bilet anahtarı kipi:** netcode tarzı, FS yok. Şimdi yok; istenirse aynı
  kayıt katmanına ikinci el sıkışma kipi olarak eklenir.
- **Adres doğrulama jetonu:** QUIC Retry/NEW_TOKEN gibi, yeniden bağlanmada
  çerez turunu atlamak için.

## 11. Dış güvenlik incelemesi (karar 8)

**Ne zaman:** B5a bittikten sonra, kayıt katmanı rUDP'ye bağlıyken.

**Kapsam:**
1. `crates/gsb-net/src/seal/` tamamı (test hariç ~1070 satır, yorumsuz ~690): nonce
   kuralı, replay penceresi, anahtar fazı ve önceki anahtarın bırakılma
   kuralı, bütünlük sınırı, ret sırası, anahtar silme (zeroize).
2. NK'nin çerez el sıkışmasına binişi:
   - DH'nin çerezden sonra olması;
   - prologue bağlamı;
   - idempotent proof (saklanan msg2);
   - sahte accept toleransı.
3. B5a'nın demux/writer bağlaması:
   - CID → oturum yönlendirmesi;
   - göç kuralının üç koşulu;
   - PATH_* ve 3x bütçe;
   - ACK → sayaç eşlemesi (`note_peer_ack`).
4. Stateless reset (B5b): döngü ve amplifikasyon, sabit zamanlı
   karşılaştırma.
5. C# portu geldiğinde: aynı vektörlerle uyum (cacophony NK + bizim
   SEALED vektörlerimiz).

**Kapsam dışı (hazır bileşenler):**
- `snow`: denetlenmemiş, tek bakımcı. Yalnız el sıkışma için kullanılır;
  gerekirse `clatter`'a ya da ~200 satırlık kendi NK'mize geçilir.
- RustCrypto AEAD: NCC Group 2019–20 incelemesi, bulgu yok.

## 12. Tur sırası: B3 → B89 → B5a → B5b → B7

| Tur | Ne yapar |
|---|---|
| **x1 (bu tur)** | **Yapıldı:** `gsb_net::seal` çekirdeği (el sıkışma sarmalayıcısı, `Sealer`/`Opener`, replay penceresi, anahtar fazı, reset jetonu, SEALED başlık kodlaması) + bu doküman. **Bağlanmadı:** rUDP'nin hiçbir yolu bu modülü çağırmaz |
| **B3** | **Yapıldı (2026-10-02):** kriptosuz CID ve göç (opt-in `udp_migration`): proof'a caps baytı, accept'te CID, etiketli c→s datagramı, PATH_CHALLENGE/RESPONSE, 3x bütçe, oturumlara iç anahtar + `addr→key` / `cid→key` indeksleri, writer'a `UDP_PATH` (`PathChanged`), `UdpClient::rebind()`, 15 sayaç; kind haritası kesin (§5). Ayrıntı §7, DESIGN §6 "Bağlantı göçü" |
| **B89** | Kaynak adres başına el sıkışma oranı ve oturum sınırı; göçte sayımın taşınması; DH'den önce global el sıkışma bütçesi. Kriptodan **önce** gelir: DH ve AEAD maliyetini o korur |
| **B5a** | **Bu modülü bağlar:** msg1 proof'a, msg2 accept'e; SEALED kayıt; sunucu statik anahtarı config'den; `Sealer` writer'a, `Opener` demux'a; `Refusal` adları sayaçlara; göç kuralının üç koşulu; PATH_* şifreli iç kind; mühürlü kip varsayılan, düz metin dev/LAN anahtarı; demux'ta çözme CPU'sunun ölçümü (100k'da) |
| **B5b** | Anahtar fazı politikası (ne zaman rekey; ACK → `note_peer_ack` eşlemesi), stateless reset (config anahtarı, reset datagramı, istemci kontrolü), opsiyonel CID rotasyonu / adres doğrulama jetonu |
| **B7** | Şifreli resume e2e: reset ya da göç başarısızlığından sonra yeni el sıkışma + RECONNECT'in resume yolu, rUDP üstünde uçtan uca |

## 13. Bu turun çekirdeği (`crates/gsb-net/src/seal/`)

**Modüller:**

| Dosya | İçerik |
|---|---|
| `mod.rs` | Modül belgesi, dışa açılanlar |
| `identity.rs` | `NOISE_PATTERN`, boy sabitleri, `StaticKey`, `Accept`, `HandshakeError` |
| `handshake.rs` | `Initiator` (istemci), `Msg1` → `Responded` (sunucu), `Session::into_halves` |
| `key.rs` | Faz anahtarı: Noise nonce kodlaması, mühürle/aç, REKEY |
| `sealer.rs` | `Sealer`, `SEAL_LIMIT`, `REKEY_MIN_DISTANCE`, `SealError` |
| `opener.rs` | `Opener`, `Refusal`, `Opened`, `INTEGRITY_LIMIT` |
| `replay.rs` | `REPLAY_WINDOW` bit halkası |
| `reset.rs` | `ResetKey`, `ResetToken` (sabit zamanlı karşılaştırma) |
| `wire.rs` | SEALED başlık sabitleri ve kodlama/çözme |

**Bağımlılıklar** (hepsi saf Rust; `ring` yalnız mevcut quinn/rustls
yolundan gelir, yenilerden hiçbiri getirmez):

| Crate | Sürüm | Lisans | Not |
|---|---|---|---|
| `snow` | 0.10.0 | Apache-2.0 OR MIT | `default-features = false`; yalnız `use-curve25519`, `use-chacha20poly1305`, `use-blake2`, `use-getrandom`, `risky-raw-split`. Varsayılan `std` özelliği `"ring/std"` der ve `ring`'i çeker. `default-resolver-crypto` da `ring`'siz, ama AES-GCM ve SHA-2'yi boşuna getirirdi |
| `chacha20poly1305` | 0.10.1 | Apache-2.0 OR MIT | snow'un kullandığı hat: tek kopya derlenir |
| `blake2` | 0.10.6 | MIT OR Apache-2.0 | snow zaten çekiyor |
| `hmac` | 0.12.1 | MIT OR Apache-2.0 | `SimpleHmac<Blake2s256>` |
| `subtle` | 2.6.1 | BSD-3-Clause | Kilitte zaten var |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | Kilitte zaten var |
| *(geçişli)* `curve25519-dalek` 4.1.3, `getrandom` 0.3.4, `chacha20` 0.9.1, `poly1305` 0.8.0, `fiat-crypto` 0.2.9 | | BSD-3 / MIT / Apache | C yok, `cc` yok |

**Testler** (`cargo test -p gsb-net --lib seal`):
- Bilinen-cevap testleri:
  - RFC 8439 §2.8.2;
  - cacophony `Noise_NK_25519_ChaChaPoly_BLAKE2s` vektörü bizim
    sarmalayıcımızdan: msg1, msg2, handshake hash, dört taşıma mesajı,
    statik açık anahtar türetimi;
  - REKEY = snow'un `rekey_outgoing`'i.
- El sıkışma: gidiş-dönüş, yanlış sabitlenmiş anahtar, bağlam uyuşmazlığı,
  boy retleri DH'den önce, sahte/yabancı accept'ten sonra tamamlanma.
- Kayıt:
  - her ret adı;
  - her bayt × bit çevirmesi;
  - pencere kenarları, bit halkasının sarması, uzak sıçrama;
  - sayaç sınırı, bütünlük sınırı;
  - faz sınırında sırasızlık, önceki anahtarın tam sınırda bırakılması.
- Seeded model testi (SplitMix64): sırasızlık, bekletme, düşürme, çoğaltma,
  sahteleme. Her hüküm anahtar bilmeyen bir modelle karşılaştırılır.

## 14. Kaynaklar

- **Noise:** https://noiseprotocol.org/noise.html (§4.2 ve §11.3 REKEY, §12.3
  ChaChaPoly nonce'u, §7.7 yük güvenliği), https://github.com/mcginty/snow
- **WireGuard:** https://www.wireguard.com/protocol/
- **RFC'ler:**
  - RFC 8439 (ChaCha20-Poly1305);
  - RFC 9000 §10.3 (stateless reset), §12.3, §8.2 (yol doğrulaması);
  - RFC 9001 §6 (anahtar güncelleme), §6.6 (kullanım sınırları);
  - RFC 9146 §6 (CID ile adres güncelleme);
  - RFC 9147; RFC 9853; RFC 6479 (replay penceresi).
- **DTLS adayları:**
  - https://github.com/algesten/dimpl
  - https://github.com/webrtc-rs/rtc
  - https://github.com/rustls/rustls/issues/40
- **RustCrypto denetimi:**
  https://www.nccgroup.com/research-blog/public-report-rustcrypto-aesgcm-and-chacha20pluspoly1305-implementation-review/
- **netcode:** https://github.com/mas-bandwidth/netcode/blob/main/STANDARD.md
- **C#:** https://github.com/bcgit/bc-csharp
