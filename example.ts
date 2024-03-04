import { C, Data, fromHex, fromText, toHex, toText } from "translucent-cardano"
import * as plutus from "../plutus"
import { Oracle, ParamsDatumType } from "../src"
//import * as WebSocket from "ws"

export interface PricedAsset {
  name: string
  fetch: () => Promise<number>
}

const syntheticScriptHash =
  "6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e"
const PARAMS_PREFIX = "p_"

const params = [
  {
    synthetic: "USDb",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a0023e86ca1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea146705f5553446201028201d81859012ad8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44454e4353ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf454c454e4649ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf434d494effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44534e454bffff9f0b0c0e0c0b0cff0a1a05f5e10019046a9f192710192710192710192710192710192710ff1932c8190bb8192328ffffff",
    oref: "73597756fc39521db7428c1745f36194ecc384b769bdf22e5d75ecc96e547ae40",
  },
  {
    synthetic: "USDs",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a00245e46a1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea146705f5553447301028201d818590131d8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44444a4544ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4455534462ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4469555344ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf456d79555344ffff9f186e187818651869186e1865ff18641a05f5e10018c89f192710192710192710192710192710192710ff1932c81913881926acffffff",
    oref: "3bbaeec9194986e0d13af38fc6a4d8876abb69688fa7254c31cebfa5b6b825de0",
  },
  {
    synthetic: "EURb",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a001e730aa1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea146705f4555526201028201d81858d8d8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534462ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534473ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effff9f183718371833183cff18321a05f5e10018c89f192710192710192710192710ff192af81913881926acffffff",
    oref: "147c26c8c2d3ffc9831a0f97554722273878e8358b18013067e0a1163795e0cf0",
  },
  {
    synthetic: "JPYb",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a001e730aa1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea146705f4a50596201028201d81858d8d8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534462ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534473ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effff9f183718371833183cff18321a05f5e10018c89f192710192710192710192710ff192af81913881926acffffff",
    oref: "1067d4e735a42b4a4859185848dd24df4a9118faa248b2a84f9348d71b0ec2d90",
  },
  {
    synthetic: "BTCb",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a002a27d6a1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea146705f4254436201028201d818590189d8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534462ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534473ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44454e4353ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf454c454e4649ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf434d494effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44534e454bffff9f183718371833183c1846183c1837183cff18321a0003d09019046a9f192710192710192710192710192710192710192710192710ff1932c819138819251cffffff",
    oref: "e8160ab05968122e9a2adce54dd7027eda197c708f13ad4145a10e439981a7370",
  },
  {
    synthetic: "MATICb",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a00272162a1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea148705f4d415449436201028201d818590159d8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4442544362ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44454e4353ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf454c454e4649ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf434d494effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44534e454bffff9f18371833183c1846183c1837183cff18320119046a9f192710192710192710192710192710192710192710ff1932c81907d019251cffffff",
    oref: "b49b6878f1765247e95395626430a0b1f18767ffbbb2bfda2fb875edf05ce5330",
  },
  {
    synthetic: "SOLp",
    output_cbor:
      "a300581d7046229ad0413ba06307c0194bdcee999ed639318ddbf47f930d7c3ee101821a002a6b2ea1581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061ea146705f534f4c7001028201d81859018dd8799fd8799fd87a9f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061effffd8799fd8799f9fd8799f4040ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534462ffd8799f581c6c6011f808562c80d7d24b9462e3eec41bea9d96e3e7a51e620a061e4455534473ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf4342544effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44454e4353ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf454c454e4649ffd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf434d494effd8799f581cd441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf44534e454bffff9f183718371833183c1846183c1837183cff18321b000000e8d4a510001905dc9f192710192710192710192710192710192710192710192710ff194e201907d0192134ffffff",
    oref: "5d35fc297bc6798d39261e75ccf933b0eac03f819c6cad051963144a4b85fc800",
  },
]

const realMappings = {
  LENFI: "8fef2d34078659493ce161a6c7fba4b56afefa8535296a5743f6958741414441",
  iUSD: "f66d78b4a3cb3d37afa0ec36461e51ecbde00f26c8f0a68f94b6988069555344",
  MIN: "29d222ce763455e3d7a09a665ce554f00ac89d2e99a1a83d267170c64d494e",
  SNEK: "279c909f348e533da5808898f87f9a14bb2c3dfbbacccd631d927a3f534e454b",
  ENCS: "9abf0afd2f236a19f2842d502d0450cbcd9c79f123a9708f96fd9b96454e4353",
  DJED: "8db269c3ec630e06ae29f74bc39edd1f87c819f1056206e879a1cd61446a65644d6963726f555344"
}

export const tokenDecimals: Record<string, number> = {
  USDb: 6,
  USDs: 6,
  EURb: 6,
  JPYb: 6,
  BTCb: 8,
  MATICb: 6,
  SOLp: 9,
  ADA: 6,
  BTN: 6,
  MIN: 6,
  SNEK: 0,
  LENFI: 6,
  ENCS: 6,
  iUSD: 6,
  myUSD: 6,
  DJED: 6,
}

const prices: Record<string, number> = {
  USDb: 1,
  USDs: 1,
  EURb: 1.0931,
  JPYb: 0.006900120752113162,
  BTCb: 45944.3,
  MATICb: 0.8206,
  SOLp: 0.010185788787483703,
  lovelace: 0.5109,
  BTN: 1.001,
  LENFI: 3.79,
  MIN: 0.03457,
  SNEK: 0.001194,
  ENCS: 0.5923,
  ADA: 0.5235,
  DJED: 1,
  myUSD: 1,
  iUSD: 0.79,
}

// for (const key of Object.keys(prices)){
//   if (!(params.find((x)=>x.synthetic==key))){
//     prices[key] = prices[key] * 0.85
//   }
// }

function getSyntheticPrice(synth: string) {
  if (prices[synth]) {
    return prices[synth]
  }
  throw new Error(`Could not find price of ${synth}`)
}

function getCollateralPrice(collateralAsset: string) {
  if (prices[collateralAsset]) {
    return prices[collateralAsset]
  }
  throw new Error(`Could not find price of ${collateralAsset}`)
}

export function getLiterateAssetName(asset: {
  policyId: string
  assetName: string
}): string {
  if (
    asset.policyId == "d441227553a0f1a965fee7d60a0f724b368dd1bddbc208730fccebcf"
  ) {
    return toText(asset.assetName)
  }
  if (asset.policyId == syntheticScriptHash) {
    return toText(asset.assetName)
  }
  if (asset.policyId == "") {
    return "lovelace"
  }
  throw new Error(
    `Could not get literate asset name of ${JSON.stringify(asset)}`,
  )
}

function gcdCalculator(a: number, b: number): number {
  if (b === 0) {
    return a
  } else {
    return gcdCalculator(b, a % b)
  }
}

function decimalToBigIntPairSigFig(
  num: number,
  sigFig: number,
  minPrecision: number,
): [bigint, bigint] {
  const precision = Math.max(sigFig - Math.floor(Math.log10(num)), minPrecision)
  const denominator = BigInt(10) ** BigInt(precision)
  const numerator = BigInt(Math.floor(num * Math.pow(10, precision)))
  return [numerator, denominator]
}

function flattenDenominator(list: [bigint, bigint][]): [bigint[], bigint] {
  let commonDenominator = BigInt(1)
  for (const [, denominator] of list) {
    commonDenominator *= denominator
  }
  const numerators = list.map(
    ([numerator, denominator]) => numerator * (commonDenominator / denominator),
  )
  let gcd = numerators[0]
  for (let i = 1; i < numerators.length; i++) {
    gcd = BigInt(gcdCalculator(Number(gcd), Number(numerators[i])))
  }
  gcd = BigInt(gcdCalculator(Number(gcd), Number(commonDenominator)))
  commonDenominator /= gcd
  for (let i = 0; i < numerators.length; i++) {
    numerators[i] /= gcd
  }
  return [numerators, commonDenominator]
}

//for (let p=0; p<30; p++) {
async function buildPayloads() {
  const p = 6
  const syntheticNames = []
  const allPayloads: plutus.PriceFeedFeedType["_redeemer"] = []
  const oracle = new Oracle({
    key: process.env.PRICE_FEED_KEY,
  })
  for (const param of params) {
    const out = C.TransactionOutput.from_bytes(fromHex(param.output_cbor))
    const outJS = out.to_js_value()
    const tokensHere = outJS.amount.multiasset![syntheticScriptHash]
    let synthetic = ""
    for (const token of Object.keys(tokensHere!)) {
      if (token.startsWith(fromText(PARAMS_PREFIX))) {
        synthetic = toText(token.slice(fromText(PARAMS_PREFIX).length))
        break
      }
    }
    const datum = Data.from(
      toHex(out.datum()!.as_inline_data()!.to_bytes()),
      plutus.SyntheticsTypes["_datum"],
    )
    const thisParams: ParamsDatumType =
      typeof datum.other != "string" && "ParamsWrapper" in datum.other
        ? datum.other.ParamsWrapper.params
        : ((new Error("couldn't get params") as unknown) as ParamsDatumType)
    const sPrice = decimalToBigIntPairSigFig(getSyntheticPrice(synthetic), p, p)
    sPrice[1] = sPrice[1] * BigInt(10 ** tokenDecimals[synthetic])
    const cPricesPre = thisParams.collateralAssets.map((x) => {
      const name = getLiterateAssetName(x)
      //console.log(name)
      const [n, d] = decimalToBigIntPairSigFig(getCollateralPrice(name), p, p)
      return [
        n * sPrice[1],
        d *
          sPrice[0] *
          10n ** BigInt(tokenDecimals[name == "lovelace" ? "ADA" : name]),
      ] as [bigint, bigint]
    })
    const cPrices = flattenDenominator(cPricesPre)
    const payload: plutus.PriceFeedFeedInnerType["_redeemer"] = {
      collateralPrices: cPrices[0],
      synthetic: fromText(synthetic),
      denominator: cPrices[1],
      validity: {
        lowerBound: {
          boundType: "NegativeInfinity",
          isInclusive: false,
        },
        upperBound: {
          boundType: "PositiveInfinity",
          isInclusive: false,
        },
      },
    }
    const signedPayload: plutus.PriceFeedFeedType["_redeemer"] = [
      await oracle.signFeed(payload),
    ]
    syntheticNames.push(synthetic)
    allPayloads.push(signedPayload[0])
  }
  const zipped = syntheticNames.map((synthetic, i) => {
    return {
      synthetic,
      price: prices[synthetic],
      payload: Data.to([allPayloads[i]], plutus.PriceFeedFeedType["_redeemer"]),
    }
  })
  //console.log(JSON.stringify(zipped))
  return zipped
}

// console.log(await buildPayloads())

// const replacer = (key: any, value: any) =>
//   typeof value === "bigint" ? value.toString() : value
// console.log(JSON.stringify(Data.from("9fd8799fd8799f9fc24908b56434a9e79dbf31c249110ba7b981dfce6489c249110ba7b981dfce6489c249111003b4094c7816f0ff44455552621b00013899d5c5eff3d8799fd8799fd87980d87980ffd8799fd87b80d87980ffffff5840363c5330e08fedf7f374a2877020a2b93c8b2bc9956337127ed18728008740cf3c7835a382deea38b4e2a984ed8b82547d50742de91344592a21babe9c13a009ffff", plutus.PriceFeedFeedType["_redeemer"]), replacer))

// const max = Math.min(...allPayloads.map((x)=>Data.to([x], plutus.PriceFeedFeedType["_redeemer"]).length / 2))
// console.log(p, `${max/16384*100}%`)
//console.log(p, `${(Data.to(allPayloads, plutus.PriceFeedFeedType["_redeemer"]).length / 2) / 16384 * 100}%`)
//}

async function pushPayloads() {
  const payloads = await buildPayloads()
  // console.log(JSON.stringify(payloads))
  //payloads = [payloads[0]]
  const response = await fetch(
    "https://infra-integration.silver-train-1la.pages.dev/api/updatePrices",
    {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify(payloads),
    },
  )
  if (prices["SOLb"]){
    prices["SOLp"] = 1/prices["SOLb"]
  }
  const data = toText(toHex((await response.body?.getReader().read())!.value!))
  //console.log(data)
}
//pushPayloads()
setInterval(pushPayloads, 10000)

function onMessage(event){
  const x = event.data
  const priceData: {
    stream: string
    data: {
      e: string
      E: number
      s: string
      p: string
      P: string
      i: string
      r: string
      T: number
    }
  } = JSON.parse(x)
  const synth =
    priceData.stream.split("@")[0].slice(0, -4).toLocaleUpperCase() + "b"
  prices[synth] = Number(priceData.data.p)
  console.log(`Setting ${synth} to ${priceData.data.p}`)
}

let ws = new WebSocket(
  `wss://fstream.binance.com/stream?streams=btcusdt@markPrice/adausdt@markPrice/solusdt@markPrice/maticusdt@markPrice`,
)
ws.addEventListener("open", console.log)
ws.addEventListener("close", ()=>{
  ws = new WebSocket(
    `wss://fstream.binance.com/stream?streams=btcusdt@markPrice/adausdt@markPrice/solusdt@markPrice/maticusdt@markPrice`,
  )
  ws.addEventListener("message", onMessage)
})
ws.addEventListener("message", onMessage)

async function getMaestroPrice(cnt: string, ada: number) {
  const response = await fetch(
    `https://mainnet.gomaestro-api.org/v1/markets/dexs/ohlc/minswap/ADA-${cnt}?limit=1&resolution=1m`,
    {
      method: "get",
      headers: {
        Accept: "application/json",
        "api-key": process.env.MAESTRO_API_KEY,
      },
    },
  )

  const data: {
    timestamp: string
    count: number
    coin_a_open: number
    coin_a_high: number
    coin_a_low: number
    coin_a_close: number
    coin_a_volume: number
    coin_b_open: number
    coin_b_high: number
    coin_b_low: number
    coin_b_close: number
    coin_b_volume: number
  }[] = await response.json()
  return ((data[0].coin_a_close + data[0].coin_a_open) / 2) * ada
}


setInterval(async ()=>{
  const ada = prices["ADA"]
  for (const key of Object.keys(realMappings)) {
    prices[key] = await getMaestroPrice(key, ada);
    console.log(`Setting ${key} to ${prices[key]}`)
  }
}, 10000)