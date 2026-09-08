use crate::config::DEFAULT_POSTGRES_VERSION;
use crate::error::{DBCenterError, Result};
use crate::DBCenter;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// PostGIS version info for CDN mapping
#[derive(Debug, Clone)]
struct PostGISVersion {
    version: &'static str,
    min_pg: u32,
    max_pg: u32,
}

/// pgvector version info
#[derive(Debug, Clone)]
struct ExtensionVersion {
    version: &'static str,
    min_pg: u32,
    max_pg: u32,
}

/// pgvector versions (Windows)
///
/// 0.8.1 is split around PostgreSQL 15 deliberately. That build shipped
/// `lib/vector.lib`, an import library, where every other Windows build ships
/// `lib/vector.dll` - so it has no loadable module, and CREATE EXTENSION would
/// fail after an install that looked like it worked. It is not published, and
/// PostgreSQL 15 resolves to 0.8.0, which is intact.
const PGVECTOR_WINDOWS_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion {
        version: "0.8.1",
        min_pg: 16,
        max_pg: 18,
    },
    ExtensionVersion {
        version: "0.8.1",
        min_pg: 13,
        max_pg: 14,
    },
    ExtensionVersion {
        version: "0.8.0",
        min_pg: 15,
        max_pg: 17,
    },
];

/// pgvector versions (Linux)
///
/// Ranges here have to match what is actually published, or resolution picks a
/// file name that does not exist and the user gets a 404 instead of a clear
/// "not built for your PostgreSQL".
const PGVECTOR_LINUX_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion {
        version: "0.8.1",
        min_pg: 13,
        max_pg: 18,
    },
    ExtensionVersion {
        version: "0.8.0",
        min_pg: 13,
        max_pg: 17,
    },
];

/// pghttp versions (Windows)
const PGHTTP_WINDOWS_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion {
        version: "1.7.0",
        min_pg: 18,
        max_pg: 18,
    }, // Only 18 has 1.7.0 in list
    ExtensionVersion {
        version: "1.6.3",
        min_pg: 17,
        max_pg: 17,
    },
    ExtensionVersion {
        version: "1.6.1",
        min_pg: 13,
        max_pg: 16,
    },
];

/// pghttp versions (Linux)
const PGHTTP_LINUX_VERSIONS: &[ExtensionVersion] = &[ExtensionVersion {
    version: "1.7.0",
    min_pg: 12,
    max_pg: 18,
}];

/// Available PostGIS versions on CDN for Windows
/// postgis-3.6.1: pg13-18
/// postgis-3.5.0: pg16-17
/// postgis-3.4.2: pg12
const POSTGIS_WINDOWS_VERSIONS: &[PostGISVersion] = &[
    PostGISVersion {
        version: "3.6.1",
        min_pg: 13,
        max_pg: 18,
    },
    PostGISVersion {
        version: "3.5.0",
        min_pg: 16,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.4.2",
        min_pg: 12,
        max_pg: 12,
    },
];

/// Available PostGIS versions on CDN for Linux
/// postgis-3.5.0: pg12-17
/// postgis-3.4.3: pg12-17
/// postgis-3.3.7: pg12-17
/// postgis-3.2.7: pg12-15
/// postgis-3.1.11: pg12-15
const POSTGIS_LINUX_VERSIONS: &[PostGISVersion] = &[
    // PostGIS gained PostgreSQL 18 support in 3.6. Everything older fails to
    // compile against it, which is why the default configuration - PostgreSQL 18
    // - could not install PostGIS on Linux at all until this build existed.
    PostGISVersion {
        version: "3.6.1",
        min_pg: 18,
        max_pg: 18,
    },
    PostGISVersion {
        version: "3.5.0",
        min_pg: 12,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.4.3",
        min_pg: 12,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.3.7",
        min_pg: 12,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.2.7",
        min_pg: 12,
        max_pg: 15,
    },
    PostGISVersion {
        version: "3.1.11",
        min_pg: 12,
        max_pg: 15,
    },
];

/// Where extension bundles are fetched from.
///
/// They used to come from a CDN this project ran, with `?token=admin-secret-key`
/// baked into every client - a shared secret sitting in everyone's binary,
/// guarding a service that had to stay up or the feature stopped existing. It is
/// down as of this writing, which is exactly the failure mode. GitHub Releases
/// needs no secret and no server, and is fronted by a CDN that is somebody
/// else's problem to keep running.
///
/// The assets live in a separate public repository because this one is private,
/// and GitHub serves release assets from a private repository only to
/// authenticated callers - which a user's machine, downloading anonymously, is
/// not.
const DEFAULT_ASSET_BASE_URL: &str =
    "https://github.com/heyobi/dbcenter-extensions/releases/download/extensions-v1";

/// SHA-256 of each published bundle, keyed by file name.
///
/// These archives are unpacked into the PostgreSQL installation and their shared
/// libraries are loaded into the server process, so a substituted one is
/// arbitrary code execution. Pinning the digests here means the host serving
/// them is not trusted: a tampered asset fails to verify even if the account
/// publishing it is compromised.
///
/// Add an entry per uploaded asset. A download whose name is not listed is
/// refused; the error reports the digest it computed, which is the value to pin.
const EXTENSION_CHECKSUMS: &[(&str, &str)] = &[
    (
        "pghttp-1.3.1-linux-amd64-pg12.tar.gz",
        "e945e39fd132a621a8bac0053d13a19db46dca386efe9edf066387d0fa4e599a",
    ),
    (
        "pghttp-1.3.1-linux-amd64-pg13.tar.gz",
        "49c98cb853b38e543cb767184ac46e743cfb93ad2dd184038add24591b08b795",
    ),
    (
        "pghttp-1.3.1-linux-amd64-pg14.tar.gz",
        "52a50e74edef7ae8e656999a994f6e118b5ee32ece3ecb73646c170a50528191",
    ),
    (
        "pghttp-1.3.1-linux-amd64-pg15.tar.gz",
        "5e5d9a86255b2c9a406ebe10da40d0cc40dd55484f49c4fa04c05a4f149c6e5f",
    ),
    (
        "pghttp-1.3.1-linux-amd64-pg16.tar.gz",
        "38c80a48feb241b9755d19a95c9e6c46300703331f43f98a9756d943a02d79ec",
    ),
    (
        "pghttp-1.3.1-linux-amd64-pg17.tar.gz",
        "f0d90cdbf0f9341ff25ef3dcee39e9a71d4f673171cf3a465f658d5a5d52131c",
    ),
    (
        "pghttp-1.4.0-linux-amd64-pg14.tar.gz",
        "576853c51b36df367fa9a38609bb3ca8aa20207eb7f81dd9639d24250f6b3b5d",
    ),
    (
        "pghttp-1.4.0-linux-amd64-pg15.tar.gz",
        "a2edb57374160e3b478071ad1f5291f97919658056243ef105c01c895943c3a0",
    ),
    (
        "pghttp-1.4.1-linux-amd64-pg12.tar.gz",
        "80b818ae3b7bfd83ec5dfdb2884d6c1468344936eafb8589ce993cf7adb724c1",
    ),
    (
        "pghttp-1.4.1-linux-amd64-pg13.tar.gz",
        "9941f1af9fedc7105e38453effd7b9ddf393c254bcca0904e573f58068cc2042",
    ),
    (
        "pghttp-1.4.1-linux-amd64-pg14.tar.gz",
        "fac5c4d5c5de8f77eb936d3cbbad3d2f9266bd08923ddf50a49034acd85aa356",
    ),
    (
        "pghttp-1.4.1-linux-amd64-pg15.tar.gz",
        "49434dba7c22a6dd0365a9a9a6d728add9c230a5fa41fe92d674485d60b588ad",
    ),
    (
        "pghttp-1.5.0-linux-amd64-pg12.tar.gz",
        "a4aa0a19e12358ec2d76dbd5ef652bbe4ee1b7fa4b41e879b0c3719a4d511db3",
    ),
    (
        "pghttp-1.5.0-linux-amd64-pg13.tar.gz",
        "3df850ce4dd05bc83ad151ec04dc37a3088c252634b9227a1134c4c4ce43a3f6",
    ),
    (
        "pghttp-1.5.0-linux-amd64-pg14.tar.gz",
        "a589f1014da2ac127bc7f95aff3a9556cadac6d4ca4aa3cf84c44fe23a0c7f23",
    ),
    (
        "pghttp-1.5.0-linux-amd64-pg15.tar.gz",
        "173da5357e31e809649aee9f12a37b9aee9d24529ff930821ec26b19893aafee",
    ),
    (
        "pghttp-1.6.0-linux-amd64-pg12.tar.gz",
        "c13cfd1b52da8d1af7304ea97a3c8c1e487e5cb91ccbee658f72ec8f4fdf4dde",
    ),
    (
        "pghttp-1.6.0-linux-amd64-pg13.tar.gz",
        "c47a4ac766d117a2cfca311e7c345c91cfc682fcd61b488c248e9187f4f1f5b4",
    ),
    (
        "pghttp-1.6.0-linux-amd64-pg14.tar.gz",
        "08eb657963baee74e21a201afbc23d6bd85b91ecf663743691db6e1c11fc00ba",
    ),
    (
        "pghttp-1.6.0-linux-amd64-pg15.tar.gz",
        "b1a6f0bfc8d38dde7c29ae89840b3a3110c344488b759aa6593a91ed87468d79",
    ),
    (
        "pghttp-1.6.0-linux-amd64-pg16.tar.gz",
        "c2c6d042ac05f52148430465338aa061052f023cdd31faf6e8758eef49974c82",
    ),
    (
        "pghttp-1.6.0-linux-amd64-pg17.tar.gz",
        "b884937d34a26d885aff83e57f8ae4f75f27dc6333a82729d31f11e8a73b45a1",
    ),
    (
        "pghttp-1.6.1-linux-amd64-pg12.tar.gz",
        "2f6561a2a90c35220709a78da64c3f9f31f2826b362b5eb1716bb4485f397f1c",
    ),
    (
        "pghttp-1.6.1-linux-amd64-pg13.tar.gz",
        "955e621773c255614f64f83efe87997f8ae265880f0433e58e4949eaee0c41b3",
    ),
    (
        "pghttp-1.6.1-linux-amd64-pg14.tar.gz",
        "b3c5c254c391db7355b1985e820c1fefed2fdbaf584a668d26edd74d69f03e5e",
    ),
    (
        "pghttp-1.6.1-linux-amd64-pg15.tar.gz",
        "a17fc56a6fe5297ad2f51427beaf11567166d2522bee46e64bdaa7d0132b94fb",
    ),
    (
        "pghttp-1.6.1-linux-amd64-pg16.tar.gz",
        "b05e4fc2ed1528a273691b4f84cca06136f8d46ed4bc044f302f5c3174603bc9",
    ),
    (
        "pghttp-1.6.1-linux-amd64-pg17.tar.gz",
        "2c12a727e009c181c4569e9c132669ef14c416ca94d38e8f2729a8cc2a7f61a2",
    ),
    (
        "pghttp-1.6.1-windows-amd64-pg13.zip",
        "524b03f8c26774a48240656cb798c23b36d823c89932639be7a5d1a985c005af",
    ),
    (
        "pghttp-1.6.1-windows-amd64-pg14.zip",
        "bac7c838a3b0ffeff6ea3a9906399ac7fde8eb0fd06644ebcc3d5764ee2bf9ec",
    ),
    (
        "pghttp-1.6.1-windows-amd64-pg15.zip",
        "d98d4147b5e13452c8a10d6ee55c17ffcf30862ac796ff79702496273c37b98c",
    ),
    (
        "pghttp-1.6.1-windows-amd64-pg16.zip",
        "a909be79d3b4075234854a7fd7641d4787322ea3e39e73aa1c1afdd7673f252d",
    ),
    (
        "pghttp-1.6.2-linux-amd64-pg12.tar.gz",
        "1df31fc9b4bdcf7ad149817082b2455f4496a165e7f1c7bd1a4596b28564acd4",
    ),
    (
        "pghttp-1.6.2-linux-amd64-pg13.tar.gz",
        "d9a9968d698c33cf4d42874143a02bacddde7b07b3365b8d3611a42d2601551a",
    ),
    (
        "pghttp-1.6.2-linux-amd64-pg14.tar.gz",
        "654c5f0fe665cff9e5d4b39afc343cc5a1e53180e36dd650ddaf788172d1e413",
    ),
    (
        "pghttp-1.6.2-linux-amd64-pg15.tar.gz",
        "fa078d99d8220292fe14bb7908c19d6d3492fb7fc12660ab2820043e58bd193d",
    ),
    (
        "pghttp-1.6.2-linux-amd64-pg16.tar.gz",
        "40586522a8b70626ed8450352a635e2baaaa13d8d62950d17d610278992110db",
    ),
    (
        "pghttp-1.6.2-linux-amd64-pg17.tar.gz",
        "6a41a857f36f6f6b9832a91fdfb8ec338b0e39c7c5d4e3f5c12a7aba1665bcb4",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg12.tar.gz",
        "f7a377027ee8753c4c1f3be1f5b9ecd5c2e6c218a50c1187e6e5c6eae31a9af3",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg13.tar.gz",
        "ddcc9ca42865b8b688e1742265adac98be48aea9456166569a202ab880305cc5",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg14.tar.gz",
        "728faf95a58e82210bc20ada9789fb2bc7ac3b9c0a95a8d126698a561eadc6e2",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg15.tar.gz",
        "aef23efdacf826b0657d14680775355c2432d8231849a15e12890bd273d1683a",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg16.tar.gz",
        "5ef23f2ac9d6f6708e28e1dc04b14ba4817545f01cbef9909898af437c8ecc5d",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg17.tar.gz",
        "fe9f2c298a1ee548e2f8f2b51d8af0269aa5bdb46117bd50b65a85aa8eded40b",
    ),
    (
        "pghttp-1.6.3-linux-amd64-pg18.tar.gz",
        "870fd81448dff7d9777003704d79f9fa7b75b4067b776753f3c204e09703eb2b",
    ),
    (
        "pghttp-1.6.3-windows-amd64-pg17.zip",
        "17beb1757de90e5b596684dd3d56eca37421e05516749c2560aa6c465b9f08a6",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg12.tar.gz",
        "3fbe80c749f177f6eb72a5af98d8ae49234f2054c85beab39c7b74e8a6b0162c",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg13.tar.gz",
        "a2408fef7fd85ccd950e2c975afb2acfc92f1c5bc8660ba7678f6aa0ce86941c",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg14.tar.gz",
        "973655c28f6065042c17f53d5c1f4e69e5bbd611b1059343707e8e8a68cd8005",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg15.tar.gz",
        "41d9e7ff1bcc6abf6a815c313ecb7cfd0b1229ce0951ebb23d6826be27d2b6ff",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg16.tar.gz",
        "26292bd08cd8194327645388fc7e161687aab2482659709fad6e516f49412b96",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg17.tar.gz",
        "2419f5e3fc77c9125d3dc924b2f6c0ca48035812b1069438ae45aa2ce2087b78",
    ),
    (
        "pghttp-1.7.0-linux-amd64-pg18.tar.gz",
        "21fd4d98f3ddd9b9005148c1c244b69b78bcc3491eaa34cdd4ddd25f430c96c5",
    ),
    (
        "pghttp-1.7.0-windows-amd64-pg18.zip",
        "2c31b8ca3981791a34f440e62b03580e4785d9fdf4a14927f58e3350a72b881a",
    ),
    (
        "pgvector-0.5.1-linux-amd64-pg13.tar.gz",
        "0abc8500ff0415a9866d9c0a242ed06e8d0af4017e40ed382948d38a6a73f0b6",
    ),
    (
        "pgvector-0.5.1-linux-amd64-pg14.tar.gz",
        "bcc68f4c16ec6f094022d142808f5697465a036a8b039da2ed3dd9010abcde74",
    ),
    (
        "pgvector-0.5.1-linux-amd64-pg15.tar.gz",
        "3ca348e833b4dabd322d0358ff7286a10d06da680ea0688992e9b9ea1e059122",
    ),
    (
        "pgvector-0.5.1-linux-amd64-pg16.tar.gz",
        "fa6543016e433f9b64b5b03844e38cdd8de205253b88a8442e9f443fd9fadf38",
    ),
    (
        "pgvector-0.5.1-linux-amd64-pg17.tar.gz",
        "d200eab15dccfcaff843a5089e6d5669fa042f72f3b119c34fed596826a4a8df",
    ),
    (
        "pgvector-0.6.0-linux-amd64-pg13.tar.gz",
        "ff322bbc85972640d4b30977005a57fbd59aec0f77b6e1a3f5644750ac4e4f9c",
    ),
    (
        "pgvector-0.6.0-linux-amd64-pg14.tar.gz",
        "067d49c7ea51e1d532486e0c9d8163a9fdbb6e7c62adebaeed7bd8942fb6f53d",
    ),
    (
        "pgvector-0.6.0-linux-amd64-pg15.tar.gz",
        "4e77856ddd4e610fb6de058ec74fd0679aebcc3be4df22723d4f35393ba7c25d",
    ),
    (
        "pgvector-0.6.0-linux-amd64-pg16.tar.gz",
        "0fc2ac964106fdc70eb961682092ab36a57e4990815a5899ddd01d4c61c6af0a",
    ),
    (
        "pgvector-0.6.0-linux-amd64-pg17.tar.gz",
        "1b9ea7445d7be36b7fbaa748e760a96a66bca8c4061993c5c48a6f24bb0fd46f",
    ),
    (
        "pgvector-0.7.0-linux-amd64-pg13.tar.gz",
        "4462d31df6af1fb15fd5aab4e78b44cf812900d004adc717433d4ec44e1ec0c7",
    ),
    (
        "pgvector-0.7.0-linux-amd64-pg14.tar.gz",
        "1c70965c2c796ff9efb9f51cbe53ed0dfacefca863ec16df108d8a4ebbd4a80d",
    ),
    (
        "pgvector-0.7.0-linux-amd64-pg15.tar.gz",
        "875e18ad59488007c3328c1b275f50263697e8465c12d28d4ed26b36de1be22b",
    ),
    (
        "pgvector-0.7.0-linux-amd64-pg16.tar.gz",
        "7405c1be99acbc6142faf8c815c6deccdbed891e0f400a65b0e1d0e515074033",
    ),
    (
        "pgvector-0.7.0-linux-amd64-pg17.tar.gz",
        "5bb7240d568e04ce1042d1487d47252598f43360d6614147c1f34739781e8605",
    ),
    (
        "pgvector-0.7.4-linux-amd64-pg13.tar.gz",
        "dbf27d804f35582ed204919de1917decef604f69bdb550d64584d8abf0a33d34",
    ),
    (
        "pgvector-0.7.4-linux-amd64-pg14.tar.gz",
        "ea0ed3db5b57cb29cc6cd530255505a0b0cdb8e8dbeb2cbd66e040a2f1dea634",
    ),
    (
        "pgvector-0.7.4-linux-amd64-pg15.tar.gz",
        "86e0707c0480eeb758696a7bbd317c0527635963017f42fb5bdb304b3fced511",
    ),
    (
        "pgvector-0.7.4-linux-amd64-pg16.tar.gz",
        "01f1ed1eb82044c842749db3ad26091cadc8bec56dbd0f67274f7e60559f0720",
    ),
    (
        "pgvector-0.7.4-linux-amd64-pg17.tar.gz",
        "ee7a535620787717d0a0aa0226c1015b26c20e0eb29ae56283cd24ae07481050",
    ),
    (
        "pgvector-0.8.0-linux-amd64-pg13.tar.gz",
        "fab45756a7e2f8c6ddf4583fcf0c009c2ed9b6d00e93d9948a1d67966eb3bbbe",
    ),
    (
        "pgvector-0.8.0-linux-amd64-pg14.tar.gz",
        "9d756daabd6007e36bd31bb173263540c94513ea73a14ff37c9f0377e1383d8d",
    ),
    (
        "pgvector-0.8.0-linux-amd64-pg15.tar.gz",
        "7cff7dfc07f6f93c62e7017d360b283bd32355e83c4bd39d4a993561fddbff9b",
    ),
    (
        "pgvector-0.8.0-linux-amd64-pg16.tar.gz",
        "cce452c33bf410accc2a4a45d42381ee182d911b06dd1a3236a985e980bf039f",
    ),
    (
        "pgvector-0.8.0-linux-amd64-pg17.tar.gz",
        "f6b566896af46fff734581aa91bf3a324024112f5a9b40eb1cb901bebc6f63cf",
    ),
    (
        "postgis-3.1.11-linux-amd64-pg12.tar.gz",
        "7a2ec2ddb9854b1593d28b594aaea99a495535a1b2afbfa7512e7381e8f4c136",
    ),
    (
        "postgis-3.1.11-linux-amd64-pg13.tar.gz",
        "4f905c3bcc6056d30968e8b5185b51a3c16ae7ff4ae0db4dfa7cecd20084b57a",
    ),
    (
        "postgis-3.1.11-linux-amd64-pg14.tar.gz",
        "f1a97cbab92901c06dcb2efb3bdeb426e2a24f144486855aca9c6e8fe69e0b9e",
    ),
    (
        "postgis-3.1.11-linux-amd64-pg15.tar.gz",
        "28b8b07f32e24901cc5d68c503750c018d4d4af2f07bda29af8b8f16ca58cc5a",
    ),
    (
        "postgis-3.2.7-linux-amd64-pg12.tar.gz",
        "df0ede4b01daa256ef517b7cd40f56381f6f209bf8a1025fa97d75d9d679111e",
    ),
    (
        "postgis-3.2.7-linux-amd64-pg13.tar.gz",
        "a0d46f91eeff83169b06ef56dc1c5966ae02fab3deb214bdf9d221e9b588d000",
    ),
    (
        "postgis-3.2.7-linux-amd64-pg14.tar.gz",
        "831cab1e1cc43bbd9a2c6f8386035e202a2b91e93b241fff5666cbbe1e92fa13",
    ),
    (
        "postgis-3.2.7-linux-amd64-pg15.tar.gz",
        "4b690718a8ec190ff770829fdb9429367a0493167f302b640cea17236d6ebc51",
    ),
    (
        "postgis-3.3.7-linux-amd64-pg12.tar.gz",
        "47e445805058d9ee0ebe9b6d9801cc75361df7d678ebf8d883094791eef19205",
    ),
    (
        "postgis-3.3.7-linux-amd64-pg13.tar.gz",
        "af4c96f4acedee1ed93f5732ed83d2bb99f911ef898b2380152e6ed72debea5c",
    ),
    (
        "postgis-3.3.7-linux-amd64-pg14.tar.gz",
        "aa590d46d1ec2f5dde662d80c8734aff3cf11a4a0c8870acd7a376f4aa59aa27",
    ),
    (
        "postgis-3.3.7-linux-amd64-pg15.tar.gz",
        "234e500a70ee235921bb856e6808a9831e64c12bf6d867bf6db3c8665c7dcecb",
    ),
    (
        "postgis-3.3.7-linux-amd64-pg16.tar.gz",
        "99882cb0212478e3833da7a6ab3dd60cea0472737ace3691053e773308a44bbb",
    ),
    (
        "postgis-3.3.7-linux-amd64-pg17.tar.gz",
        "c457ebe370611fbaeaafdf72cbd1997ca77ad62e64f2d616dd45444b25f56ce7",
    ),
    (
        "postgis-3.4.2-windows-amd64-pg12.zip",
        "55f8a380ff2868934639dcfb86684da4b4c36b19aa96a6c3c9150da983a03296",
    ),
    (
        "postgis-3.4.3-linux-amd64-pg12.tar.gz",
        "3266ef77cc267c43b0c0510adb318085da178313071dc33601e69470df91aafb",
    ),
    (
        "postgis-3.4.3-linux-amd64-pg13.tar.gz",
        "2317ed8e3c3828ac7eb41e16f25c1713f7ed7d1e8daf4810abe0d0d60aba8d9d",
    ),
    (
        "postgis-3.4.3-linux-amd64-pg14.tar.gz",
        "e8165b9ad888e294d93f319a5f740ce95ac92dddba0261038af23679f6f852d5",
    ),
    (
        "postgis-3.4.3-linux-amd64-pg15.tar.gz",
        "d03ad3cee5ac6cf20cc19a0a40192207eb5bd54b9393939c4f33d594aee31c6a",
    ),
    (
        "postgis-3.4.3-linux-amd64-pg16.tar.gz",
        "91ae5fe25431ef56ed0660b21f1ae029550fcaa62cc48735cc2ba2ad7ad00c1a",
    ),
    (
        "postgis-3.4.3-linux-amd64-pg17.tar.gz",
        "b689ad4c773d60d69e332b5ccc9bef187263d360a78890a34cc54e289f5ba255",
    ),
    (
        "postgis-3.5.0-linux-amd64-pg12.tar.gz",
        "1179c7636bf3b44e90fad65a6ad98a0aee32ad2ef52e33e092bd6a050a803083",
    ),
    (
        "postgis-3.5.0-linux-amd64-pg13.tar.gz",
        "be92e6bdd3e46e5106934abbe5ef4e8afbb1341d0c0631aa1d1542e142486f84",
    ),
    (
        "postgis-3.5.0-linux-amd64-pg14.tar.gz",
        "91307002492744021cb664df7d15513b02419085b6d756f14a0bac23a6f19c78",
    ),
    (
        "postgis-3.5.0-linux-amd64-pg15.tar.gz",
        "0e1adf5d6f71b197a3971a344d92fe90040e5b362bfc8d4731307502142e6a44",
    ),
    (
        "postgis-3.5.0-linux-amd64-pg16.tar.gz",
        "7d43f7ebd5a5ffd37efdb0033b3e27a9a95e6faeac2652f5c4a202edbc6285cf",
    ),
    (
        "postgis-3.5.0-linux-amd64-pg17.tar.gz",
        "1af27887886d327c564c274b75f39f2abe97188d9a25b78957db9dee596a07ac",
    ),
    (
        "postgis-3.5.0-windows-amd64-pg16.zip",
        "66e5f9d70c35217cd373cce6cf29209bb7746230e8873b04fa081316462852be",
    ),
    (
        "postgis-3.5.0-windows-amd64-pg17.zip",
        "89ae63c961b303de374a1ab7c4d011921e8643205860fd3f1f7238cc55516d0a",
    ),
    (
        "postgis-3.6.1-linux-amd64-pg18.tar.gz",
        "873c873ab40a5ed6f153e0cebe89530ef64ef20b347f618aa46c1616808b05f3",
    ),
    (
        "postgis-3.6.1-windows-amd64-pg13.zip",
        "8bbf2a5892ee8313342b61563b3bf1ca121e108670c8aea3ef7992be884a9d0d",
    ),
    (
        "postgis-3.6.1-windows-amd64-pg14.zip",
        "8700ec327b2bdb456207c478e16f1cbbe9f20d71bd46b77ee87a9639a0b53cbb",
    ),
    (
        "postgis-3.6.1-windows-amd64-pg15.zip",
        "161e275af1912974012a386b932ea169be00b7a563c3a1664ee2b1a721a6d45c",
    ),
    (
        "postgis-3.6.1-windows-amd64-pg16.zip",
        "0ec6ade387bf2a0c3d5ca2cd325bbc7538b2c886ab66854d0acfe39b959a0678",
    ),
    (
        "postgis-3.6.1-windows-amd64-pg17.zip",
        "bd2ebdc917b5afbcf45de34c814f2d948b3044e546e3cb6be78b22c4c620c854",
    ),
    (
        "postgis-3.6.1-windows-amd64-pg18.zip",
        "f078c28df5217834bb568a8a6161c1d960bb94b85e8489afe97ba344be80a610",
    ),
    (
        "vector-0.5.1-linux-amd64-pg13.tar.gz",
        "de83d8496e0f204cb9b1cd664b3551228e03288555dfe349414683e221dc97d7",
    ),
    (
        "vector-0.5.1-linux-amd64-pg14.tar.gz",
        "8d6dda1e85da58f430076d15bc1e899b1f509f87bb970a87d943397dacad3eb8",
    ),
    (
        "vector-0.5.1-linux-amd64-pg15.tar.gz",
        "654a3bd753eeb4f2b4028d898238bf7e88af2c49ca3d9f543aedaa2a213d9511",
    ),
    (
        "vector-0.5.1-linux-amd64-pg16.tar.gz",
        "4ca315d2dc8a0f0add1e2f3dd8921169a80b2b76ee6e1c4c1bda072176b36621",
    ),
    (
        "vector-0.5.1-linux-amd64-pg17.tar.gz",
        "0a9881e031e5afbd2b6b7056e725c027ed41869c50ea1f7cda3e574058d034ae",
    ),
    (
        "vector-0.6.0-linux-amd64-pg13.tar.gz",
        "a289d44e27771f997f15ff3237eed7cd757a70b1c72df2382e4dc8cbfb046dc5",
    ),
    (
        "vector-0.6.0-linux-amd64-pg14.tar.gz",
        "b5e1fc36f277ffd1a9c7b96bfccb8a7734fcfcff5b865ed09ed325609cdb2e76",
    ),
    (
        "vector-0.6.0-linux-amd64-pg15.tar.gz",
        "d84e7ecf0998fb1675bbc5006f51067b21d67d3e79ffa74cda9e40f85a2e8f33",
    ),
    (
        "vector-0.6.0-linux-amd64-pg16.tar.gz",
        "cc13e3781475fb70052cc1b7409f920f2386ea1300b51915810dadc19d9ac134",
    ),
    (
        "vector-0.6.0-linux-amd64-pg17.tar.gz",
        "15cdc0929d229059b298819d41df1ec00850555b7145bb4e62a49a1cda43169d",
    ),
    (
        "vector-0.7.0-linux-amd64-pg13.tar.gz",
        "9a8ba0f6575e32565482bdac07a3d6d04e30e655f19357ec94075c8c50a420d6",
    ),
    (
        "vector-0.7.0-linux-amd64-pg14.tar.gz",
        "9ee06b67a4af03642cedc533ee9a1fdea047a175f3a7abf520a34eaefd657b16",
    ),
    (
        "vector-0.7.0-linux-amd64-pg15.tar.gz",
        "a485cb69e3f057f39c79eb2f0f5eeb92d618a22c9ab8e8ff6a45fcecfd25f11d",
    ),
    (
        "vector-0.7.0-linux-amd64-pg16.tar.gz",
        "52d4eb95b16db0202882152c7228e17e996557c7f45f8dc70585a55173443f69",
    ),
    (
        "vector-0.7.0-linux-amd64-pg17.tar.gz",
        "7f0b73b9b94a408b86d3293995c40e63035c8bcaebf011ace300f12e30bbbf12",
    ),
    (
        "vector-0.7.3-linux-amd64-pg13.tar.gz",
        "f05bdd456769a2c8262ecb04aba6fa295946914d753dc83fc691e28297dd6ddd",
    ),
    (
        "vector-0.7.3-linux-amd64-pg14.tar.gz",
        "f2c701273348ec9618ba9c851de5161997f14ce1262b59720f8ce23d49c6c624",
    ),
    (
        "vector-0.7.3-linux-amd64-pg15.tar.gz",
        "3a5fc29d0f7440cfe15b8c7c47884da726eca5e2a091b087dc4f29f5225646df",
    ),
    (
        "vector-0.7.3-linux-amd64-pg16.tar.gz",
        "47ec87332449bfe8c19a2e5b322257abe46def378ca7e8880ac95242fdde6d0c",
    ),
    (
        "vector-0.7.3-linux-amd64-pg17.tar.gz",
        "2f88d6a66ccfba98afb71d2923b80e92afb5af009cddeba5f4e74226c8010d76",
    ),
    (
        "vector-0.7.3-windows-amd64-pg15.zip",
        "798e7a2c252fcf219c349a414a1b255c5bc936b088dcacf78937da4df9ce3a27",
    ),
    (
        "vector-0.7.4-linux-amd64-pg13.tar.gz",
        "434a306a7b60e18ab61089fe24e27e6f5760ad3c26a03247a7585d5a5be3bf6f",
    ),
    (
        "vector-0.7.4-linux-amd64-pg14.tar.gz",
        "52b5ece328d99df30de8def09ff41f193ebfc42db435abf1bc7e322d38ecb2c4",
    ),
    (
        "vector-0.7.4-linux-amd64-pg15.tar.gz",
        "db1aa25c4fe12dada08a136e6078f24e143e5547b82d87f5342d6f267de4bcee",
    ),
    (
        "vector-0.7.4-linux-amd64-pg16.tar.gz",
        "02f422faf9d321b50031c24be48344284a3d55b70dd051919008da6b1ae783aa",
    ),
    (
        "vector-0.7.4-linux-amd64-pg17.tar.gz",
        "5d4da3f05c6a9e1beebb19da03a760798fd310cb7c3904362e3ae42ffaa9ab69",
    ),
    (
        "vector-0.7.4-windows-amd64-pg16.zip",
        "5fe4d19e0d096c7a0e448cddaf7d9b12a9e1b11bbf181266232ba75c2bbb3e6f",
    ),
    (
        "vector-0.8.0-linux-amd64-pg13.tar.gz",
        "bf33899245cc188b17ccfa13e0e789e7c147af172b25d98311c2ddeebcd0ec5a",
    ),
    (
        "vector-0.8.0-linux-amd64-pg14.tar.gz",
        "70dfad0db6bf38e01518501cc348b784fb072b32e77a7c031cfb927c3a146f6e",
    ),
    (
        "vector-0.8.0-linux-amd64-pg15.tar.gz",
        "59bda12b399c97a782e440821a59ffbd83f978a158ee909639246bf40bdb5a43",
    ),
    (
        "vector-0.8.0-linux-amd64-pg16.tar.gz",
        "892148755b9413b2bcc5c2937123f9058561f5d4d9f2c87c90e97e7f5b269e0e",
    ),
    (
        "vector-0.8.0-linux-amd64-pg17.tar.gz",
        "fe8ff25895274ddadddc7a8e9bedb623fddc634103daf8c21b4b76ff44aff8da",
    ),
    (
        "vector-0.8.0-windows-amd64-pg15.zip",
        "47445d1f2909a271b83192628073d0d012f61dfe9c36565640b348e15cc64c62",
    ),
    (
        "vector-0.8.0-windows-amd64-pg16.zip",
        "14b735e03a0a0e26bb8789bc601cd716559b1f0b9bedcfef9598c927e2983253",
    ),
    (
        "vector-0.8.0-windows-amd64-pg17.zip",
        "e4cda31cbe5d7f9c5110ab2b75256029451d26dbffcbfbec40452a3d0905a622",
    ),
    (
        "vector-0.8.1-linux-amd64-pg13.tar.gz",
        "fc562e1cee6f7d6a0586ae14e71477341bbf4d022c74b74389045d6919f06a0d",
    ),
    (
        "vector-0.8.1-linux-amd64-pg14.tar.gz",
        "d6cb7cde2046bd0ea788076997d91c8a7501481c86d498afc0ae4bc39797154a",
    ),
    (
        "vector-0.8.1-linux-amd64-pg15.tar.gz",
        "c86f6d0fd7e52d3e1d582d1057d4e2670229787a6301e9c6b66da1f7c7308e3a",
    ),
    (
        "vector-0.8.1-linux-amd64-pg16.tar.gz",
        "dbe0b3f8a737afc0556412332d46d80131c1c24617d92977795306a20a435619",
    ),
    (
        "vector-0.8.1-linux-amd64-pg17.tar.gz",
        "8b8734c46cc1b70194bb6070373245b6d32eb88c8a1bbbad19f60a10f40757dc",
    ),
    (
        "vector-0.8.1-linux-amd64-pg18.tar.gz",
        "c4eee78a6e945e9eca0b2292146f67b41c526451daba46b39d9a36dd0ed635b2",
    ),
    (
        "vector-0.8.1-windows-amd64-pg13.zip",
        "e40cb56c392a0461868c11b6da9fbf26d813daa7e31c5f6629112e80dc46f7c2",
    ),
    (
        "vector-0.8.1-windows-amd64-pg14.zip",
        "c97386bcd8a0414244ac17d8512629eb0332f864694a5e917402c075c3620fa8",
    ),
    (
        "vector-0.8.1-windows-amd64-pg16.zip",
        "d24b6347908461b173074fae790e9852e137c89bbdd115a23caaac5f8e914cbb",
    ),
    (
        "vector-0.8.1-windows-amd64-pg17.zip",
        "6367877d6863a76ed28959f6ccbacb7afba80e224f12e1000fad7960f3c7f8e0",
    ),
    (
        "vector-0.8.1-windows-amd64-pg18.zip",
        "d6150efc524f97d51131975b25fa2c78833f402b1caf94d88e39096da4338819",
    ),
];

fn asset_base_url() -> String {
    // Overridable for a fork, a local file server while testing, or an
    // air-gapped mirror.
    std::env::var("DBCENTER_EXTENSION_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_ASSET_BASE_URL.to_string())
}

fn allow_unverified_assets() -> bool {
    matches!(
        std::env::var("DBCENTER_ALLOW_UNVERIFIED_EXTENSIONS").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Check a bundle against its pinned digest, deleting it if it does not match.
///
/// A bad archive is removed rather than left in the cache, so a retry re-fetches
/// instead of failing forever on a poisoned file.
fn verify_asset(path: &Path, file_name: &str) -> Result<()> {
    verify_against(path, file_name, EXTENSION_CHECKSUMS)
}

/// The body of [`verify_asset`], with the digest table passed in so it can be
/// exercised without publishing real assets.
fn verify_against(path: &Path, file_name: &str, checksums: &[(&str, &str)]) -> Result<()> {
    let actual = sha256_file(path)?;
    let expected = checksums
        .iter()
        .find(|(name, _)| *name == file_name)
        .map(|(_, digest)| *digest);

    match expected {
        Some(expected) if expected.eq_ignore_ascii_case(&actual) => {
            tracing::debug!("Verified {}", file_name);
            Ok(())
        }
        Some(expected) => {
            let _ = fs::remove_file(path);
            Err(DBCenterError::ExtensionError(format!(
                "Checksum mismatch for '{}'. Expected {}, got {}. \
                 The download was discarded and not installed.",
                file_name, expected, actual
            )))
        }
        None if allow_unverified_assets() => {
            tracing::warn!(
                "Installing '{}' without verification (sha256 {}). \
                 Set DBCENTER_ALLOW_UNVERIFIED_EXTENSIONS only when you trust the source.",
                file_name,
                actual
            );
            Ok(())
        }
        None => {
            let _ = fs::remove_file(path);
            Err(DBCenterError::ExtensionError(format!(
                "No pinned checksum for '{}', so it was not installed. \
                 Its sha256 is {} - add that to EXTENSION_CHECKSUMS to publish it, \
                 or set DBCENTER_ALLOW_UNVERIFIED_EXTENSIONS=1 to accept it unverified.",
                file_name, actual
            )))
        }
    }
}

/// Fetch one extension bundle into the cache and verify it.
///
/// Verification runs on a cache hit too, not only on a fresh download: otherwise
/// anything that could write to the cache directory could bypass it entirely.
fn fetch_extension_asset(cache_dir: &Path, file_name: &str, description: &str) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)?;
    let path = cache_dir.join(file_name);

    if path.exists() {
        tracing::info!("Using cached {}", description);
    } else {
        let url = format!("{}/{}", asset_base_url().trim_end_matches('/'), file_name);
        download_with_progress(&url, &path, description)?;
    }

    verify_asset(&path, file_name)?;
    Ok(path)
}

/// Install PostgreSQL extension
pub fn install_extension(db: &DBCenter, name: &str) -> Result<()> {
    install_extension_with_version(db, name, None)
}

/// Install PostgreSQL extension with optional version
pub fn install_extension_with_version(
    db: &DBCenter,
    name: &str,
    version: Option<&str>,
) -> Result<()> {
    match name.to_lowercase().as_str() {
        "postgis" => install_postgis(db, version),
        "pgvector" => install_pgvector(db, version),
        "pghttp" | "pgsql_http" | "http" => install_pghttp(db, version),
        "pg_trgm" | "pgtrgm" => install_bundled_extension(db, "pg_trgm"),
        "pg_cron" | "pgcron" => {
            tracing::warn!("pg_cron support coming soon");
            Err(DBCenterError::ExtensionError(
                "pg_cron extension not yet implemented".to_string(),
            ))
        }
        _ => Err(DBCenterError::ExtensionError(format!(
            "Unknown extension: {}. Supported extensions: postgis, pgvector, pghttp, pg_trgm, pg_cron (coming soon)",
            name
        ))),
    }
}

/// Install PostGIS extension with optional version
fn install_postgis(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    // Check if already installed
    let mut client = db.get_client();

    match client.query("SELECT PostGIS_Version() as version", &[]) {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                let version: String = row.get(0);
                tracing::info!("PostGIS already installed: {}", version);
                return Ok(());
            }
        }
        Err(_) => {
            // Not installed, proceed with installation
        }
    }

    drop(client); // Release lock before downloading

    tracing::info!("Installing PostGIS extension...");

    if cfg!(windows) {
        install_postgis_windows(db, requested_version)?;
    } else {
        install_postgis_unix(db, requested_version)?;
    }

    // Activate extension in database
    let mut client = db.get_client();

    // First check if postgis.control exists
    let pg_root = get_postgres_root(db)?;
    let control_path = pg_root
        .join("share")
        .join("extension")
        .join("postgis.control");
    if !control_path.exists() {
        // Try alternate path
        let alt_control = pg_root
            .join("share")
            .join("postgresql")
            .join("extension")
            .join("postgis.control");
        if !alt_control.exists() {
            return Err(DBCenterError::ExtensionError(format!(
                "PostGIS control file not found. Expected at: {} or {}. The extension files may not have been extracted correctly.",
                control_path.display(), alt_control.display()
            )));
        }
    }

    match client.execute("CREATE EXTENSION IF NOT EXISTS postgis", &[]) {
        Ok(_) => {}
        Err(e) => {
            // Get detailed error info
            let err_msg = format!("{:?}", e);
            return Err(DBCenterError::ExtensionError(format!(
                "Failed to create extension: {}. Details: {}",
                e, err_msg
            )));
        }
    }

    tracing::info!("PostGIS extension activated");

    // Try optional extensions
    let _ = client.execute("CREATE EXTENSION IF NOT EXISTS postgis_raster", &[]);
    let _ = client.execute("CREATE EXTENSION IF NOT EXISTS postgis_topology", &[]);

    // Verify installation
    let rows = client
        .query("SELECT PostGIS_Version() as version", &[])
        .map_err(|e| {
            DBCenterError::ExtensionError(format!(
                "PostGIS verification failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

    if let Some(row) = rows.first() {
        let version: String = row.get(0);
        tracing::info!("PostGIS {} successfully installed!", version);
    }

    Ok(())
}

/// Install pgvector extension
fn install_pgvector(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    install_simple_extension(
        db,
        "vector",
        "pgvector",
        PGVECTOR_WINDOWS_VERSIONS,
        PGVECTOR_LINUX_VERSIONS,
        requested_version,
        "vector", // filename prefix
    )
}

/// Install pghttp extension
fn install_pghttp(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    install_simple_extension(
        db,
        "http",
        "pghttp",
        PGHTTP_WINDOWS_VERSIONS,
        PGHTTP_LINUX_VERSIONS,
        requested_version,
        "pghttp", // filename prefix
    )
}

/// Generic installer for bundled extensions (like pg_trgm)
fn install_bundled_extension(db: &DBCenter, ext_name: &str) -> Result<()> {
    let mut client = db.get_client();
    let create_sql = format!("CREATE EXTENSION IF NOT EXISTS \"{}\"", ext_name);

    match client.execute(&create_sql, &[]) {
        Ok(_) => {
            tracing::info!("Extension '{}' activated successfully.", ext_name);
            Ok(())
        }
        Err(e) => {
            tracing::error!("Failed to install bundled extension '{}': {}", ext_name, e);
            let err_msg = format!("{:?}", e);
            Err(DBCenterError::ExtensionError(format!(
                "Failed to create bundled extension '{}'. Ensure standard PostgreSQL contrib modules are installed. Details: {}",
                ext_name, err_msg
            )))
        }
    }
}

/// Generic installer for standard zip/tar extensions (like pgvector, pghttp)
fn install_simple_extension(
    db: &DBCenter,
    ext_name: &str,    // e.g. "vector" or "http" (dbname)
    folder_name: &str, // e.g. "pgvector" (CDN folder)
    win_versions: &[ExtensionVersion],
    nix_versions: &[ExtensionVersion],
    requested_version: Option<&str>,
    file_prefix: &str, // e.g. "vector" or "pghttp"
) -> Result<()> {
    let mut client = db.get_client();
    // Check if installed using basic query
    let check_sql = format!(
        "SELECT extname FROM pg_extension WHERE extname = '{}'",
        ext_name
    );
    if let Ok(rows) = client.query(&check_sql, &[]) {
        if !rows.is_empty() {
            tracing::info!("Extension '{}' already installed.", ext_name);
            return Ok(());
        }
    }
    drop(client);

    tracing::info!("Installing {} extension...", ext_name);

    if cfg!(windows) {
        install_extension_windows(
            db,
            folder_name,
            win_versions,
            requested_version,
            file_prefix,
        )?;
    } else {
        install_extension_unix(
            db,
            folder_name,
            nix_versions,
            requested_version,
            file_prefix,
        )?;
    }

    // Activate
    let mut client = db.get_client();
    let create_sql = format!("CREATE EXTENSION IF NOT EXISTS \"{}\"", ext_name);

    match client.execute(&create_sql, &[]) {
        Ok(_) => {
            tracing::info!("Extension '{}' activated successfully.", ext_name);
            Ok(())
        }
        Err(e) => {
            let err_msg = format!("{:?}", e);
            Err(DBCenterError::ExtensionError(format!(
                "Failed to create extension '{}': {}. Details: {}",
                ext_name, e, err_msg
            )))
        }
    }
}

fn install_extension_windows(
    db: &DBCenter,
    folder_name: &str,
    versions: &[ExtensionVersion],
    requested_version: Option<&str>,
    file_prefix: &str,
) -> Result<()> {
    let pg_version = postgres_major_version(db);
    // Reuse logic from find_best_postgis_version but need generic struct.
    // Since ExtensionVersion and PostGISVersion are identical structure, we can cast or clone logic.
    // For simplicity, I'll essentially duplicate or adapt the finder logic locally or make finder generic.
    // Let's adapt logic here for now.

    let (ver_str, target_pg) =
        find_best_extension_version(pg_version, versions, requested_version)?;

    let _ = folder_name;
    let file_name = format!(
        "{}-{}-windows-amd64-pg{}.zip",
        file_prefix, ver_str, target_pg
    );

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;
    let cache_dir = base_dir.join("cache").join("extensions");
    let zip_path = fetch_extension_asset(
        &cache_dir,
        &file_name,
        &format!("{} {} for PostgreSQL {}", file_prefix, ver_str, target_pg),
    )?;

    let pg_root = get_postgres_root(db)?;
    extract_postgis_bundle(&zip_path, &pg_root)?; // Reuse the robust extractor

    Ok(())
}

fn install_extension_unix(
    db: &DBCenter,
    folder_name: &str,
    versions: &[ExtensionVersion],
    requested_version: Option<&str>,
    file_prefix: &str,
) -> Result<()> {
    let pg_version = postgres_major_version(db);
    let (ver_str, target_pg) =
        find_best_extension_version(pg_version, versions, requested_version)?;

    let _ = folder_name;
    let file_name = format!(
        "{}-{}-linux-amd64-pg{}.tar.gz",
        file_prefix, ver_str, target_pg
    );

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;
    let cache_dir = base_dir.join("cache").join("extensions");
    let tar_path = fetch_extension_asset(
        &cache_dir,
        &file_name,
        &format!("{} {} for PostgreSQL {}", file_prefix, ver_str, target_pg),
    )?;

    let pg_root = get_postgres_root(db)?;
    extract_postgis_tarball(&tar_path, &pg_root)?;

    Ok(())
}

/// Why a build for an older major version is not a usable fallback.
///
/// PostgreSQL stamps every loadable module with a magic block carrying the major
/// version it was compiled against, and refuses to load one that disagrees. So
/// fetching a pg17 build for a pg18 server cannot work; it only spends the
/// download and turns a clear "not published yet" into "incompatible library"
/// somewhere deep in CREATE EXTENSION.
fn unsupported_pg(extension: &str, version: &str, pg_version: u32, max_pg: u32) -> DBCenterError {
    DBCenterError::ExtensionError(format!(
        "{} {} has no build for PostgreSQL {}; the newest published is for \
         PostgreSQL {}. Run DBCenter with .postgres_version(\"{}\") to use it, \
         or wait for a {} build.",
        extension, version, pg_version, max_pg, max_pg, pg_version
    ))
}

fn find_best_extension_version(
    pg_version: u32,
    versions: &[ExtensionVersion],
    requested_version: Option<&str>,
) -> Result<(&'static str, u32)> {
    if let Some(req_ver) = requested_version {
        for v in versions {
            if v.version == req_ver {
                if pg_version >= v.min_pg && pg_version <= v.max_pg {
                    return Ok((v.version, pg_version));
                } else if pg_version > v.max_pg {
                    return Err(unsupported_pg(req_ver, v.version, pg_version, v.max_pg));
                }
            }
        }
        return Err(DBCenterError::ExtensionError(format!(
            "Requested version {} not found",
            req_ver
        )));
    }

    for v in versions {
        if pg_version >= v.min_pg && pg_version <= v.max_pg {
            return Ok((v.version, pg_version));
        }
    }

    if let Some(v) = versions.first() {
        if pg_version > v.max_pg {
            return Err(unsupported_pg(
                "This extension",
                v.version,
                pg_version,
                v.max_pg,
            ));
        }
    }

    Err(DBCenterError::ExtensionError(format!(
        "No compatible version for PG {}",
        pg_version
    )))
}

/// Find the best PostGIS version for the given PostgreSQL version
fn find_best_postgis_version(
    pg_version: u32,
    versions: &[PostGISVersion],
    requested_version: Option<&str>,
) -> Result<(&'static str, u32)> {
    // If a specific version is requested, try to use it
    if let Some(req_ver) = requested_version {
        for v in versions {
            if v.version == req_ver {
                if pg_version >= v.min_pg && pg_version <= v.max_pg {
                    return Ok((v.version, pg_version));
                } else if pg_version > v.max_pg {
                    return Err(unsupported_pg("PostGIS", v.version, pg_version, v.max_pg));
                } else {
                    return Err(DBCenterError::ExtensionError(format!(
                        "PostGIS {} requires PostgreSQL {} or newer, but you have PostgreSQL {}.",
                        req_ver, v.min_pg, pg_version
                    )));
                }
            }
        }
        return Err(DBCenterError::ExtensionError(format!(
            "Requested PostGIS version {} not available on CDN.",
            req_ver
        )));
    }

    // Auto-select best version
    // First, try to find an exact match
    for v in versions {
        if pg_version >= v.min_pg && pg_version <= v.max_pg {
            tracing::info!(
                "Selected PostGIS {} for PostgreSQL {}",
                v.version,
                pg_version
            );
            return Ok((v.version, pg_version));
        }
    }

    if let Some(v) = versions.first() {
        if pg_version > v.max_pg {
            return Err(unsupported_pg("PostGIS", v.version, pg_version, v.max_pg));
        }
    }

    // If PostgreSQL version is too old
    Err(DBCenterError::ExtensionError(format!(
        "PostgreSQL {} is not supported. Minimum required: PostgreSQL 12.",
        pg_version
    )))
}

/// Download file with progress bar
fn download_with_progress(url: &str, dest_path: &Path, description: &str) -> Result<()> {
    use reqwest::blocking::Client;
    use std::io::Write;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600)) // 10 minute timeout for large files
        .build()
        .map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to create HTTP client: {}", e))
        })?;

    let mut response = client
        .get(url)
        .send()
        .map_err(|e| DBCenterError::ExtensionError(format!("Download failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(DBCenterError::ExtensionError(format!(
            "Download failed with status: {}. URL: {}",
            response.status(),
            url
        )));
    }

    let total_size = response.content_length().unwrap_or(0);

    // Create progress bar
    let pb = if total_size > 0 {
        let pb = ProgressBar::new(total_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        pb.set_message(format!("Downloading {}", description));
        Some(pb)
    } else {
        tracing::info!("Downloading {} (size unknown)...", description);
        None
    };

    // Download with streaming
    let mut file = fs::File::create(dest_path)?;
    let mut downloaded: u64 = 0;
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = response.read(&mut buffer).map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to read response: {}", e))
        })?;

        if bytes_read == 0 {
            break;
        }

        file.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;

        if let Some(ref pb) = pb {
            pb.set_position(downloaded);
        }
    }

    if let Some(pb) = pb {
        pb.finish_with_message(format!("{} downloaded successfully!", description));
    }

    tracing::info!("{} downloaded: {} bytes", description, downloaded);

    Ok(())
}

/// Install PostGIS on Windows
fn install_postgis_windows(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    tracing::info!("Downloading PostGIS bundle for Windows...");

    let pg_version = postgres_major_version(db);
    let (postgis_version, target_pg_version) =
        find_best_postgis_version(pg_version, POSTGIS_WINDOWS_VERSIONS, requested_version)?;

    let file_name = format!(
        "postgis-{}-windows-amd64-pg{}.zip",
        postgis_version, target_pg_version
    );

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;
    let cache_dir = base_dir.join("cache").join("extensions");
    let zip_path = fetch_extension_asset(
        &cache_dir,
        &file_name,
        &format!(
            "PostGIS {} for PostgreSQL {}",
            postgis_version, target_pg_version
        ),
    )?;

    // Extract to PostgreSQL directory
    let pg_root = get_postgres_root(db)?;
    extract_postgis_bundle(&zip_path, &pg_root)?;

    Ok(())
}

/// Install PostGIS on Unix (Linux/macOS)
fn install_postgis_unix(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    tracing::info!("Installing PostGIS for Unix/Linux...");

    let pg_version = postgres_major_version(db);
    let (postgis_version, target_pg_version) =
        find_best_postgis_version(pg_version, POSTGIS_LINUX_VERSIONS, requested_version)?;

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;

    let file_name = format!(
        "postgis-{}-linux-amd64-pg{}.tar.gz",
        postgis_version, target_pg_version
    );

    let cache_dir = base_dir.join("cache").join("extensions");
    let tar_path = fetch_extension_asset(
        &cache_dir,
        &file_name,
        &format!(
            "PostGIS {} for PostgreSQL {}",
            postgis_version, target_pg_version
        ),
    )?;

    // Extract
    let pg_root = get_postgres_root(db)?;
    extract_postgis_tarball(&tar_path, &pg_root)?;

    Ok(())
}

/// Extract PostGIS bundle (Windows)
fn extract_postgis_bundle(zip_path: &Path, pg_root: &Path) -> Result<()> {
    use zip::ZipArchive;

    tracing::info!(
        "Extracting PostGIS files from {:?} to {:?}",
        zip_path,
        pg_root
    );

    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| DBCenterError::ExtensionError(format!("Failed to open ZIP: {}", e)))?;

    // First pass: check if there is a common root directory
    let mut common_root: Option<String> = None;
    let mut has_common_root = true;

    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i) {
            let name = file.name();
            // Skip paths ending with / (directories) for root check,
            // unless they are the only thing.
            // Better: get the first component.
            let parts: Vec<&str> = name.split('/').collect();
            if parts.is_empty() {
                continue;
            }

            let first_part = parts[0];

            // If we haven't set a common root yet
            if common_root.is_none() {
                common_root = Some(first_part.to_string());
            } else {
                if common_root.as_deref() != Some(first_part) {
                    has_common_root = false;
                    break;
                }
            }
        }
    }

    // If we only have files at root (len 1 parts), common_root might set to filename.
    // If has_common_root is true, we must ensure that common_root is actually a directory
    // that contains EVERY file.
    // If the zip contains just "foo.txt", common_root="foo.txt". We shouldn't strip it.
    // So we should only strip if the zip ENTRIES imply a directory structure where everything
    // is under `dir/`.
    // Actually, simple heuristic: if has_common_root is true, we strip the first component.
    // BUT we must be careful not to strip files if the "root" is just a file itself?
    // In a standard zip with a folder, you have `folder/`, `folder/file`.
    // If you have `file1`, `file2`, common root mismatch.
    // If you have `folder/file1`, `folder/file2`. Common root `folder`.
    // Using `strip_prefix` boolean.

    let strip_prefix = has_common_root && common_root.is_some();
    if strip_prefix {
        tracing::info!("Detected common root '{:?}', stripping it.", common_root);
    } else {
        tracing::info!("No common root detected, extracting as is.");
    }

    // Since ZipArchive consumes the file in our variable, and we can't easily reset it without seeking?
    // ZipArchive doesn't expose underlying reader easily for seek.
    // We should re-open the file.
    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| DBCenterError::ExtensionError(format!("Failed to reopen ZIP: {}", e)))?;

    let mut file_count = 0;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to read ZIP entry: {}", e))
        })?;

        let name = file.name().to_string();
        let parts: Vec<&str> = name.split('/').collect();

        // Determine target path
        let target_path = if strip_prefix {
            if parts.len() <= 1 {
                continue;
            }
            pg_root.join(parts[1..].join("/"))
        } else {
            pg_root.join(&name)
        };

        if file.is_dir() {
            fs::create_dir_all(&target_path)?;
        } else {
            // Perform extraction
            // Check if we need to reorganize the file (e.g. DLLs at root)
            // This happens if zip is flat OR if zip has a container folder with flat files inside.
            // We use component count to robustly detect if the file is directly under pg_root
            let is_root_dll = name.to_lowercase().ends_with(".dll")
                && target_path.components().count() == pg_root.components().count() + 1;

            if is_root_dll {
                let filename = Path::new(&name).file_name().unwrap_or_default();

                // Extract directly to lib (avoiding root lock)
                let lib_target = pg_root.join("lib").join(filename);
                if let Some(parent) = lib_target.parent() {
                    fs::create_dir_all(parent)?;
                }

                match fs::File::create(&lib_target) {
                    Ok(mut outfile) => {
                        if let Err(e) = std::io::copy(&mut file, &mut outfile) {
                            let is_lock_error = e.raw_os_error() == Some(32); // Windows sharing violation
                            if is_lock_error {
                                tracing::warn!("Skipping locked file (lib): {} ({})", name, e);
                            } else {
                                return Err(DBCenterError::ExtensionError(format!(
                                    "Failed to write to lib {}: {}",
                                    name, e
                                )));
                            }
                        } else {
                            file_count += 1;
                        }
                    }
                    Err(e) => {
                        let is_lock_error = e.raw_os_error() == Some(32);
                        if is_lock_error {
                            tracing::warn!("Skipping locked file (lib): {} ({})", name, e);
                        } else {
                            return Err(DBCenterError::ExtensionError(format!(
                                "Failed to create file in lib {}: {}",
                                name, e
                            )));
                        }
                    }
                }

                // Copy to bin
                let bin_target = pg_root.join("bin").join(filename);
                if let Some(parent) = bin_target.parent() {
                    fs::create_dir_all(parent)?;
                }
                // ignore copy errors to bin if locked (unlikely since we just created lib file)
                // actually we copy FROM lib_target.
                // If lib_target skipped, we might fail here?
                if lib_target.exists() {
                    if let Err(e) = fs::copy(&lib_target, &bin_target) {
                        let is_lock_error = e.raw_os_error() == Some(32);
                        if is_lock_error {
                            tracing::warn!("Skipping locked file (bin): {} ({})", name, e);
                        } else {
                            tracing::warn!("Failed to copy to bin {}: {}", name, e);
                        }
                    }
                }
            } else {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)?;
                }

                match fs::File::create(&target_path) {
                    Ok(mut outfile) => {
                        if let Err(e) = std::io::copy(&mut file, &mut outfile) {
                            let is_lock_error = e.raw_os_error() == Some(32);
                            if is_lock_error {
                                tracing::warn!("Skipping locked file: {} ({})", name, e);
                            } else {
                                return Err(DBCenterError::ExtensionError(format!(
                                    "Failed to extract {}: {}",
                                    name, e
                                )));
                            }
                        } else {
                            file_count += 1;
                        }
                    }
                    Err(e) => {
                        let is_lock_error = e.raw_os_error() == Some(32);
                        if is_lock_error {
                            tracing::warn!("Skipping locked file: {} ({})", name, e);
                        } else {
                            return Err(DBCenterError::ExtensionError(format!(
                                "Failed to create {}: {}",
                                name, e
                            )));
                        }
                    }
                }
            }
        }
    }

    tracing::info!("Extracted {} PostGIS files", file_count);
    Ok(())
}

/// Extract PostGIS tarball (Unix)
fn extract_postgis_tarball(tar_path: &Path, pg_root: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    tracing::info!(
        "Extracting extension tarball from {:?} to {:?}",
        tar_path,
        pg_root
    );

    // Detect target layout quirks
    // Linux builds often use share/postgresql/extension instead of share/extension
    let use_share_postgresql = pg_root.join("share").join("postgresql").exists();
    let use_lib_postgresql = pg_root.join("lib").join("postgresql").exists();

    if use_share_postgresql {
        tracing::debug!("Detected share/postgresql layout, will adjust extension paths.");
    }
    if use_lib_postgresql {
        tracing::debug!("Detected lib/postgresql layout, will adjust library paths.");
    }

    // First pass: detect common root
    let tar_file = fs::File::open(tar_path)?;
    let decompressor = GzDecoder::new(tar_file);
    let mut archive = Archive::new(decompressor);

    let mut common_root: Option<String> = None;
    let mut has_common_root = true;
    let mut entries_count = 0;

    for entry in archive
        .entries()
        .map_err(|e| DBCenterError::ExtensionError(format!("Failed to read tar entries: {}", e)))?
    {
        let file = entry.map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to read tar entry: {}", e))
        })?;
        let path = file
            .path()
            .map_err(|e| DBCenterError::ExtensionError(format!("Failed to get path: {}", e)))?;
        let path_str = path.to_string_lossy();

        let parts: Vec<&str> = path_str.split('/').collect();
        if parts.is_empty() {
            continue;
        }

        let first_part = parts[0];

        if common_root.is_none() {
            common_root = Some(first_part.to_string());
        } else if common_root.as_deref() != Some(first_part) {
            has_common_root = false;
            break;
        }
        entries_count += 1;
    }

    if entries_count == 0 {
        return Err(DBCenterError::ExtensionError("Empty tarball".to_string()));
    }

    let strip_prefix = has_common_root && common_root.is_some();
    if strip_prefix {
        tracing::info!("Detected common root '{:?}', stripping it.", common_root);
    }

    // Re-open for extraction
    let tar_file = fs::File::open(tar_path)?;
    let decompressor = GzDecoder::new(tar_file);
    let mut archive = Archive::new(decompressor);

    for entry in archive
        .entries()
        .map_err(|e| DBCenterError::ExtensionError(format!("Failed to read tar entries: {}", e)))?
    {
        let mut file = entry.map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to read tar entry: {}", e))
        })?;
        let path = file
            .path()
            .map_err(|e| DBCenterError::ExtensionError(format!("Failed to get path: {}", e)))?;
        let path_str = path.to_string_lossy().to_string(); // Own the string

        let mut parts: Vec<&str> = path_str.split('/').collect();

        if strip_prefix {
            if parts.len() <= 1 {
                continue; // Skip the root dir itself
            }
            parts = parts[1..].to_vec();
        }

        // Handle layout adjustments

        if parts.is_empty() {
            continue;
        }

        let final_path_buf = if parts[0] == "share" && use_share_postgresql {
            // Inject "postgresql" -> share/postgresql/...
            let mut new_parts = vec!["share", "postgresql"];
            new_parts.extend_from_slice(&parts[1..]);
            pg_root.join(new_parts.join("/"))
        } else if parts[0] == "lib" && use_lib_postgresql {
            // Inject "postgresql" -> lib/postgresql/...
            let mut new_parts = vec!["lib", "postgresql"];
            new_parts.extend_from_slice(&parts[1..]);
            pg_root.join(new_parts.join("/"))
        } else {
            pg_root.join(parts.join("/"))
        };

        let target_path = final_path_buf;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        file.unpack(&target_path).map_err(|e| {
            DBCenterError::ExtensionError(format!(
                "Failed to unpack to {}: {}",
                target_path.display(),
                e
            ))
        })?;
    }

    tracing::info!("Extension files extracted");
    Ok(())
}

/// Get PostgreSQL root directory
fn get_postgres_root(db: &DBCenter) -> Result<std::path::PathBuf> {
    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;

    // Check for pgsql subdirectory
    let pgsql_dir = base_dir.join("pgsql");
    if pgsql_dir.exists() {
        Ok(pgsql_dir)
    } else {
        Ok(base_dir)
    }
}

/// Get PostgreSQL major version
fn postgres_major_version(db: &DBCenter) -> u32 {
    let fallback = DEFAULT_POSTGRES_VERSION
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(18);

    db.postgres_version()
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256 of the three bytes "abc", the standard test vector.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("dbcenter_asset_test_{}", name));
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn digest_matches_the_known_vector() {
        let path = temp_file("digest", b"abc");
        assert_eq!(sha256_file(&path).unwrap(), ABC_SHA256);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn an_unpinned_asset_is_refused_and_its_digest_reported() {
        // Refusing by default is the point: these archives are unpacked into the
        // PostgreSQL installation and loaded into the server process.
        let path = temp_file("unpinned", b"abc");
        let err = verify_asset(&path, "not-in-the-table.tar.gz").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("No pinned checksum"), "{message}");
        // The digest has to be in the message, or there is no way to pin it.
        assert!(message.contains(ABC_SHA256), "{message}");
        // A refused download must not be left in the cache.
        assert!(!path.exists(), "the rejected file should have been removed");
    }

    #[test]
    fn a_mismatched_digest_is_refused_and_the_file_removed() {
        let path = temp_file("mismatch", b"abc");
        let checksums: &[(&str, &str)] = &[("asset.tar.gz", "00".repeat(32).leak())];
        let err = verify_against(&path, "asset.tar.gz", checksums).unwrap_err();
        assert!(err.to_string().contains("Checksum mismatch"), "{err}");
        assert!(
            !path.exists(),
            "a tampered file must not survive in the cache"
        );
    }

    #[test]
    fn a_matching_digest_passes() {
        let path = temp_file("match", b"abc");
        let checksums: &[(&str, &str)] = &[("asset.tar.gz", ABC_SHA256)];
        verify_against(&path, "asset.tar.gz", checksums).unwrap();
        assert!(path.exists(), "a good file stays cached");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_too_new_postgres_is_a_clear_error_not_an_older_build() {
        // PostGIS on Linux is published up to pg17 while the default PostgreSQL
        // is 18, so this is the live case, not a hypothetical.
        let versions: &[PostGISVersion] = &[PostGISVersion {
            version: "3.5.0",
            min_pg: 12,
            max_pg: 17,
        }];

        let err = find_best_postgis_version(18, versions, None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no build for PostgreSQL 18"), "{message}");
        // The message has to say what would work, or it is a dead end.
        assert!(message.contains("17"), "{message}");

        // A version the caller asked for explicitly gets the same treatment.
        let err = find_best_postgis_version(18, versions, Some("3.5.0")).unwrap_err();
        assert!(err.to_string().contains("no build for PostgreSQL 18"));

        // And a supported pairing still resolves.
        assert_eq!(
            find_best_postgis_version(17, versions, None).unwrap(),
            ("3.5.0", 17)
        );
    }

    /// Every version the tables offer must correspond to a published asset.
    ///
    /// The tables and the release are two lists that have to agree, and nothing
    /// but this makes them. When they drifted - the Linux pgvector table
    /// claiming PostgreSQL 12 for a build that starts at 13 - the user got a
    /// 404 from a file name that was never published, instead of a clear
    /// "not built for your PostgreSQL".
    #[test]
    fn every_offered_version_has_a_published_asset() {
        let mut missing = Vec::new();

        let mut check = |prefix: &str, version: &str, platform: &str, pg: u32| {
            let suffix = if platform == "windows" {
                "zip"
            } else {
                "tar.gz"
            };
            let name = format!(
                "{}-{}-{}-amd64-pg{}.{}",
                prefix, version, platform, pg, suffix
            );
            if !EXTENSION_CHECKSUMS.iter().any(|(n, _)| *n == name) {
                missing.push(name);
            }
        };

        for (prefix, versions, platform) in [
            ("vector", PGVECTOR_LINUX_VERSIONS, "linux"),
            ("vector", PGVECTOR_WINDOWS_VERSIONS, "windows"),
            ("pghttp", PGHTTP_LINUX_VERSIONS, "linux"),
            ("pghttp", PGHTTP_WINDOWS_VERSIONS, "windows"),
        ] {
            for v in versions {
                for pg in v.min_pg..=v.max_pg {
                    check(prefix, v.version, platform, pg);
                }
            }
        }

        for (versions, platform) in [
            (POSTGIS_LINUX_VERSIONS, "linux"),
            (POSTGIS_WINDOWS_VERSIONS, "windows"),
        ] {
            for v in versions {
                for pg in v.min_pg..=v.max_pg {
                    check("postgis", v.version, platform, pg);
                }
            }
        }

        assert!(
            missing.is_empty(),
            "these are offered by the version tables but not published:\n  {}",
            missing.join("\n  ")
        );
    }

    #[test]
    fn base_url_can_be_overridden() {
        std::env::set_var("DBCENTER_EXTENSION_BASE_URL", "http://localhost:9/mirror");
        assert_eq!(asset_base_url(), "http://localhost:9/mirror");
        std::env::remove_var("DBCENTER_EXTENSION_BASE_URL");
        assert_eq!(asset_base_url(), DEFAULT_ASSET_BASE_URL);
    }
}
