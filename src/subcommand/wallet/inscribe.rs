use {
  self::batch::{Batch, BatchEntry, Batchfile, Mode},
  super::*,
  crate::subcommand::wallet::transaction_builder::Target,
  base64::{Engine as _, engine::general_purpose},
  bitcoin::{
    blockdata::{opcodes, script},
    key::PrivateKey,
    key::{TapTweak, TweakedKeyPair, TweakedPublicKey, UntweakedKeyPair},
    policy::MAX_STANDARD_TX_WEIGHT,
    psbt::Psbt,
    secp256k1::{self, constants::SCHNORR_SIGNATURE_SIZE, rand, Secp256k1, XOnlyPublicKey},
    sighash::{Prevouts, SighashCache, TapSighashType},
    taproot::Signature,
    taproot::{ControlBlock, LeafVersion, TapLeafHash, TaprootBuilder},
  },
  bitcoincore_rpc::bitcoincore_rpc_json::{GetRawTransactionResultVout, ImportDescriptors, SignRawTransactionInput, Timestamp},
  bitcoincore_rpc::Client,
  bitcoincore_rpc::RawTx,
  reqwest::{header, header::USER_AGENT},
  std::{collections::BTreeSet, io::Write},
  tempfile::tempdir,
  url::Url,
};

mod batch;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct InscriptionInfo {
  pub id: InscriptionId,
  pub location: SatPoint,
}

fn is_zero(n: &u64) -> bool {
  *n == 0
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Output {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commit: Option<Txid>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commit_hex: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commit_psbt: Option<String>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub inscriptions: Vec<InscriptionInfo>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub parent: Option<InscriptionId>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub recovery_descriptor: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reveal: Option<Txid>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reveal_hex: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reveal_psbt: Option<String>,
  #[serde(skip_serializing_if = "is_zero")]
  pub total_fees: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ParentInfo {
  destination: Address,
  id: InscriptionId,
  location: SatPoint,
  tx_out: TxOut,
}

#[derive(Debug, Parser)]
#[clap(
  group = ArgGroup::new("source")
      .required(true)
      .args(&["file", "batch"]),
)]
pub(crate) struct Inscribe {
  #[arg(
    long,
    help = "Inscribe multiple inscriptions defined in a yaml <BATCH_FILE>.",
    conflicts_with_all = &[
      "cbor_metadata", "destination", "file", "json_metadata", "metaprotocol", "parent", "postage", "reinscribe", "satpoint"
    ]
  )]
  pub(crate) batch: Option<PathBuf>,
  #[arg(
    long,
    help = "Include CBOR in file at <METADATA> as inscription metadata",
    conflicts_with = "json_metadata"
  )]
  pub(crate) cbor_metadata: Option<PathBuf>,
  #[arg(
    long,
    help = "Consider spending outpoint <UTXO>, even if it is unconfirmed or contains inscriptions"
  )]
  pub(crate) utxo: Vec<OutPoint>,
  #[arg(long, help = "Only spend outpoints given with --utxo")]
  pub(crate) coin_control: bool,
  #[arg(long, help = "Send any change output to <CHANGE>.")]
  pub(crate) change: Option<Address<NetworkUnchecked>>,
  #[arg(
    long,
    help = "Use <COMMIT_FEE_RATE> sats/vbyte for commit transaction.\nDefaults to <FEE_RATE> if unset."
  )]
  pub(crate) commit_fee_rate: Option<FeeRate>,
  #[arg(long, help = "Compress inscription content with brotli.")]
  pub(crate) compress: bool,
  #[arg(long, help = "Send inscription to <DESTINATION>.")]
  pub(crate) destination: Option<Address<NetworkUnchecked>>,
  #[arg(long, help = "Don't sign or broadcast transactions.")]
  pub(crate) dry_run: bool,
  #[arg(long, help = "Use fee rate of <FEE_RATE> sats/vB.")]
  pub(crate) fee_rate: FeeRate,
  #[arg(long, help = "Inscribe sat with contents of <FILE>.")]
  pub(crate) file: Option<PathBuf>,
  #[arg(
    long,
    help = "Include JSON in file at <METADATA> converted to CBOR as inscription metadata",
    conflicts_with = "cbor_metadata"
  )]
  pub(crate) json_metadata: Option<PathBuf>,
  #[clap(long, help = "Set inscription metaprotocol to <METAPROTOCOL>.")]
  pub(crate) metaprotocol: Option<String>,
  #[arg(long, alias = "nobackup", help = "Do not back up recovery key.")]
  pub(crate) no_backup: bool,
  #[arg(
    long,
    alias = "nolimit",
    help = "Do not check that transactions are equal to or below the MAX_STANDARD_TX_WEIGHT of 400,000 weight units. Transactions over this limit are currently nonstandard and will not be relayed by bitcoind in its default configuration. Do not use this flag unless you understand the implications."
  )]
  pub(crate) no_limit: bool,
  #[clap(long, help = "Make inscription a child of <PARENT>.")]
  pub(crate) parent: Option<InscriptionId>,
  #[clap(long, help = "Address to return parent inscription to.")]
  pub(crate) parent_destination: Option<Address<NetworkUnchecked>>,
  #[clap(long, help = "The satpoint of the parent inscription, in case it isn't confirmed yet.")]
  pub(crate) parent_satpoint: Option<SatPoint>,
  #[arg(
    long,
    help = "Amount of postage to include in the inscription. Default `10000sat`."
  )]
  pub(crate) postage: Option<Amount>,
  #[clap(long, help = "Allow reinscription.")]
  pub(crate) reinscribe: bool,
  #[arg(long, help = "Specify the reveal tx fee.")]
  pub(crate) reveal_fee: Option<Amount>,
  #[arg(long, help = "Inscribe <SATPOINT>.")]
  pub(crate) satpoint: Option<SatPoint>,
  #[clap(long, help = "Use provided recovery key instead of a random one.")]
  pub(crate) key: Option<String>,
  #[clap(long, help = "Don't make a reveal tx; just create a commit tx that sends all the sats to a new commitment. Either specify --key if you have one, or note the --key it generates for you. Implies --no-backup.")]
  pub(crate) commit_only: bool,
  #[clap(long, help = "Don't make a commit transaction; just create a reveal tx that reveals the inscription committed to by output <COMMITMENT>. Requires the same --key as was used to make the commitment. Implies --no-backup. This doesn't work if the --key has ever been backed up to the wallet. When using --commitment, the reveal tx will create a change output unless --reveal-fee is set to '0 sats', in which case the whole commitment will go to postage and fees.")]
  pub(crate) commitment: Option<OutPoint>,
  #[arg(long, help = "Make the change of the reveal tx commit to the contents of multiple inscriptions defined in a yaml <NEXT-BATCH>.")]
  pub(crate) next_batch: Option<PathBuf>,
  #[clap(long, help = "Make the change of the reveal tx commit to the contents of <NEXT-FILE>.")]
  pub(crate) next_file: Option<PathBuf>,
  #[clap(long, help = "Use <REVEAL-INPUT> as an extra input to the reveal tx. For use with `--commitment`.")]
  pub(crate) reveal_input: Vec<OutPoint>,
  #[clap(long, help = "Dump raw hex transactions and recovery keys to standard output.")]
  pub(crate) dump: bool,
  #[clap(long, help = "Do not broadcast any transactions. Implies --dump.")]
  pub(crate) no_broadcast: bool,
  #[clap(long, help = "Use <COMMIT-INPUT> as an extra input to the commit tx. Useful for forcing CPFP.")]
  pub(crate) commit_input: Vec<OutPoint>,
  #[arg(long, help = "Inscribe <SAT>.", conflicts_with = "satpoint")]
  pub(crate) sat: Option<Sat>,
  #[arg(long, help = "Don't use a local wallet. Leave the commit transaction unsigned instead.")]
  pub(crate) no_wallet: bool,
  #[arg(long, help = "Specify the vsize of the commit tx, for when we don't have a local wallet to sign with.")]
  pub(crate) commit_vsize: Option<u64>,
}

impl Inscribe {
  pub(crate) fn run(self, wallet: String, options: Options) -> SubcommandResult {
    if self.commitment.is_some() && self.key.is_none() {
      return Err(anyhow!("--commitment only works with --key"));
    }

    if self.commit_only && self.commitment.is_some() {
      return Err(anyhow!("--commit-only and --commitment don't work together"));
    }

    if self.next_batch.is_some() && self.next_file.is_some() {
      return Err(anyhow!("--next-batch and --next-file don't work together"));
    }

    if self.commit_only && self.next_batch.is_some() {
      return Err(anyhow!("--commit-only and --next-batch don't work together"));
    }

    if self.commit_only && self.next_file.is_some() {
      return Err(anyhow!("--commit-only and --next-file don't work together"));
    }

    if self.commitment.is_none() && !self.reveal_input.is_empty() {
      return Err(anyhow!("--reveal-input only works with --commitment"));
    }

    let mut no_backup = self.no_backup;
    if self.commit_only || self.commitment.is_some() {
      no_backup = true;
    }

    let mut dump = self.dump;
    let metadata = Inscribe::parse_metadata(self.cbor_metadata, self.json_metadata)?;

    if self.no_broadcast {
      dump = true;
    }

    let index = Index::open(&options)?;
    index.update()?;

    let (mut utxos, locked_utxos, runic_utxos, client) = if self.no_wallet {
      let utxos = BTreeMap::new();
      let locked_utxos = BTreeSet::new();
      let runic_utxos = BTreeSet::new();
      let client = check_version(options.bitcoin_rpc_client(None)?)?;
      (utxos, locked_utxos, runic_utxos, client)
    } else {
      let client = bitcoin_rpc_client_for_wallet_command(wallet, &options)?;

    let mut utxos = if self.coin_control {
      BTreeMap::new()
    } else if options.ignore_outdated_index {
      return Err(anyhow!(
        "--ignore-outdated-index only works in conjunction with --coin-control when inscribing"
      ));
    } else {
      get_unspent_outputs(&client, &index)?
    };

    let locked_utxos = get_locked_outputs(&client)?;

    let runic_utxos = index.get_runic_outputs(&utxos.keys().cloned().collect::<Vec<OutPoint>>())?;

    for outpoint in &self.utxo {
      utxos.insert(
        *outpoint,
        Amount::from_sat(
          client.get_raw_transaction(&outpoint.txid, None)?.output[outpoint.vout as usize].value,
        ),
      );
    }

    (utxos, locked_utxos, runic_utxos, client)
    };

    let chain = options.chain();

    let change = match self.change {
      Some(change) => Some(change.require_network(chain.network())?),
      None => None,
    };

    let postage;
    let destinations;
    let fee_utxos;
    let inscribe_on_specific_utxos;
    let inscriptions;
    let mode;
    let parent_info;
    let sat;

    let next_inscriptions = if self.next_file.is_some() {
      vec![Inscription::from_file(
        chain,
        None,
        self.next_file.unwrap(),
        self.parent,
        None,
        self.metaprotocol.clone(),
        metadata.clone(),
        self.compress,
        None,
      )?]
    } else if self.next_batch.is_some() {
      let batchfile = Batchfile::load(&self.next_batch.unwrap())?;
      let parent_info = Inscribe::get_parent_info(batchfile.parent, &index, &utxos, &client, chain, batchfile.parent_satpoint, self.no_wallet, self.parent_destination.clone())?;
      let postage = batchfile
          .postage
          .map(Amount::from_sat)
          .unwrap_or(TARGET_POSTAGE);

      batchfile.inscriptions(
        &client,
        chain,
        parent_info.as_ref().map(|info| info.tx_out.value),
        metadata.clone(),
        postage,
        self.compress,
        &mut utxos,
      )?.0
    } else {
      Vec::new()
    };

    match (self.file, self.batch) {
      (Some(file), None) => {
        parent_info = Inscribe::get_parent_info(self.parent, &index, &utxos, &client, chain, self.parent_satpoint, self.no_wallet, self.parent_destination)?;

        postage = self.postage.unwrap_or(TARGET_POSTAGE);

        inscriptions = vec![Inscription::from_file(
          chain,
          None,
          file,
          self.parent,
          None,
          self.metaprotocol.clone(),
          metadata.clone(),
          self.compress,
          None,
        )?];

        mode = Mode::SeparateOutputs;

        sat = self.sat;

        destinations = vec![match self.destination.clone() {
          Some(destination) => destination.require_network(chain.network())?,
          None => get_change_address(&client, chain)?,
        }];

        inscribe_on_specific_utxos = false;
        fee_utxos = Vec::new();
      }
      (None, Some(batch)) => {
        let batchfile = Batchfile::load(&batch)?;

        parent_info = Inscribe::get_parent_info(batchfile.parent, &index, &utxos, &client, chain, batchfile.parent_satpoint, self.no_wallet, self.parent_destination)?;

        postage = batchfile
          .postage
          .map(Amount::from_sat)
          .unwrap_or(TARGET_POSTAGE);

        (inscriptions, destinations, inscribe_on_specific_utxos, fee_utxos) = batchfile.inscriptions(
          &client,
          chain,
          parent_info.as_ref().map(|info| info.tx_out.value),
          metadata,
          postage,
          self.compress,
          &mut utxos,
        )?;

        mode = batchfile.mode;

        if batchfile.sat.is_some() && mode != Mode::SameSat {
          return Err(anyhow!("`sat` can only be set in `same-sat` mode"));
        }

        sat = batchfile.sat;
      }
      _ => unreachable!(),
    }

    let satpoint = if let Some(sat) = sat {
      if !index.has_sat_index() {
        return Err(anyhow!(
          "index must be built with `--index-sats` to use `--sat`"
        ));
      }
      match index.find(sat)? {
        Some(satpoint) => Some(satpoint),
        None => return Err(anyhow!(format!("could not find sat `{sat}`"))),
      }
    } else {
      self.satpoint
    };

    Ok(Box::new(Batch {
      commit_fee_rate: self.commit_fee_rate.unwrap_or(self.fee_rate),
      commit_only: self.commit_only,
      commit_vsize: self.commit_vsize,
      commitment: self.commitment,
      commitment_output: if self.commitment.is_some() {
        Some(client.get_raw_transaction_info(&self.commitment.unwrap().txid, None)?.vout[self.commitment.unwrap().vout as usize].clone())
      } else {
        None
      },
      destinations,
      dump,
      dry_run: self.dry_run,
      fee_utxos,
      inscribe_on_specific_utxos,
      inscriptions,
      key: self.key,
      mode,
      next_inscriptions,
      no_backup,
      no_broadcast: self.no_broadcast,
      no_limit: self.no_limit,
      no_wallet: self.no_wallet,
      parent_info,
      postage,
      reinscribe: self.reinscribe,
      reveal_fee: self.reveal_fee,
      reveal_fee_rate: self.fee_rate,
      reveal_input: self.reveal_input,
      reveal_psbt: None,
      satpoint,
    }
    .inscribe(chain, &index, &client, &locked_utxos, runic_utxos, &mut utxos, self.commit_input, change)?))
  }

  fn parse_metadata(cbor: Option<PathBuf>, json: Option<PathBuf>) -> Result<Option<Vec<u8>>> {
    if let Some(path) = cbor {
      let cbor = fs::read(path)?;
      let _value: Value = ciborium::from_reader(Cursor::new(cbor.clone()))
        .context("failed to parse CBOR metadata")?;

      Ok(Some(cbor))
    } else if let Some(path) = json {
      let value: serde_json::Value =
        serde_json::from_reader(File::open(path)?).context("failed to parse JSON metadata")?;
      let mut cbor = Vec::new();
      ciborium::into_writer(&value, &mut cbor)?;

      Ok(Some(cbor))
    } else {
      Ok(None)
    }
  }

  fn get_parent_info(
    parent: Option<InscriptionId>,
    index: &Index,
    utxos: &BTreeMap<OutPoint, Amount>,
    client: &Client,
    chain: Chain,
    satpoint: Option<SatPoint>,
    no_wallet: bool,
    destination: Option<Address<NetworkUnchecked>>,
  ) -> Result<Option<ParentInfo>> {
    if let Some(parent_id) = parent {
      let satpoint = if let Some(satpoint) = satpoint {
        satpoint
      } else {
        if let Some(satpoint) = index.get_inscription_satpoint_by_id(parent_id)? {
          satpoint
        } else {
          return Err(anyhow!(format!("parent {parent_id} does not exist")));
        }
      };

      let tx_out = index
        .get_transaction(satpoint.outpoint.txid)?
        .expect("parent transaction not found in index")
        .output
        .into_iter()
        .nth(satpoint.outpoint.vout.try_into().unwrap())
        .expect("current transaction output");

      if !no_wallet && !utxos.contains_key(&satpoint.outpoint) {
        return Err(anyhow!(format!("parent {parent_id} not in wallet")));
      }

      let destination = if no_wallet {
        chain.address_from_script(&tx_out.script_pubkey)?
      } else if let Some(destination) = destination {
        destination.require_network(chain.network())?
      } else {
        get_change_address(client, chain)?
      };
      
      Ok(Some(ParentInfo {
        destination,
        id: parent_id,
        location: satpoint,
        tx_out,
      }))
    } else {
      Ok(None)
    }
  }

  fn fetch_url_into_file(
    client: &reqwest::blocking::Client,
    url: &str,
    file: &PathBuf,
  ) -> Result<u64> {
    let mut res = client.get(url).send()?;

    if !res.status().is_success() {
      bail!(res.status());
    }

    match File::create(file) {
      Ok(mut fp) => 
        match res.copy_to(&mut fp) {
            Ok(n) => Ok(n),
            Err(x) => return Err(anyhow!("write error: {}", x)),
        }
      Err(x) => return Err(anyhow!("create file error: {}", x)),
    }
  }

  pub(crate) fn get_temporary_key(
    index: &Index,
    chain: Chain,
  ) -> Result<String> {
    let key_path = index.data_dir().join("key.txt");
    if let Err(err) = fs::create_dir_all(key_path.parent().unwrap()) {
      eprintln!("error");
      bail!("failed to create data dir `{}`: {err}", key_path.parent().unwrap().display());
    }

    match fs::read_to_string(key_path.clone()) {
      Ok(key) => Ok(key.trim_end().to_string()),
      Err(_) => {
        let secp256k1 = Secp256k1::new();
        let key_pair = UntweakedKeyPair::new(&secp256k1, &mut rand::thread_rng());
        let key = PrivateKey::new(key_pair.secret_key(), chain.network()).to_wif();
        let mut file = File::create(key_path)?;
        file.write(format!("{}\n", key).as_bytes())?;
        Ok(key)
      }
    }
  }

  pub(crate) fn inscribe_for_server(
    data: serde_json::Value,
    chain: Chain,
    index: &Index,
  ) -> Result<Output> {
    let no_wallet = true;

    if !data.is_object() {
      return Err(anyhow!("expected object, not {:?}", data));
    }

    let data = data.as_object().unwrap();

    if !data.contains_key("inscriptions") {
      return Err(anyhow!("expected object to contain `inscriptions`"));
    }

    if !data.contains_key("fees_utxos") {
      return Err(anyhow!("expected object to contain `fees_utxos`"));
    }

    let inscriptions = data.get("inscriptions").unwrap();
    let fees_utxos = data.get("fees_utxos").unwrap();

    let commit_vsize = if data.contains_key("commit_vsize") {
      let commit_vsize = data.get("commit_vsize").unwrap();
      if !commit_vsize.is_u64() {
        return Err(anyhow!("expected `commit_vsize` to be a u64, not {:?}", commit_vsize));
      }
      Some(commit_vsize.as_u64().unwrap())
    } else {
      None
    };

    let parent = if data.contains_key("parent") {
      let parent = data.get("parent").unwrap();
      if !parent.is_string() {
        return Err(anyhow!("expected `parent` to be a string, not {:?}", parent));
      }
      let parent = parent.as_str().unwrap();
      match InscriptionId::from_str(parent) {
        Ok(parent) => Some(parent),
        _ => return Err(anyhow!("expected `parent` to contain valid inscriptionid, not {:?}", parent)),
      }
    } else {
      None
    };

    if !inscriptions.is_array() {
      return Err(anyhow!("expected `inscriptions` to be an array, not {:?}", inscriptions));
    }

    if !fees_utxos.is_array() {
      return Err(anyhow!("expected `fees_utxos` to be an array, not {:?}", fees_utxos));
    }

    let inscriptions = inscriptions.as_array().unwrap();
    let fees_utxos = fees_utxos.as_array().unwrap();

    let mut entries = Vec::new();
    let tmpdir = tempdir().unwrap();
    let mut headers = header::HeaderMap::new();
    headers.insert(USER_AGENT, header::HeaderValue::from_static("ord inscribe endpoint"));
    let request_client = reqwest::blocking::Client::builder().default_headers(headers).build().unwrap();

    for (i, inscription) in inscriptions.iter().enumerate() {
      if !inscription.is_object() {
        return Err(anyhow!("expected `inscriptions` to only contain objects, not {:?}", inscription));
      }

      let inscription = inscription.as_object().unwrap();

      if !inscription.contains_key("file") {
        return Err(anyhow!("expected `inscription` to contain `file`"));
      }
      let file = inscription.get("file").unwrap();
      if !file.is_string() {
        return Err(anyhow!("expected `inscriptions[].file` to be a string, not {:?}", file));
      }
      let file = file.as_str().unwrap();
      let url = Url::parse(file)?;
      let path = PathBuf::from(url.path());
      let ext = match path.extension() {
        Some(ext) => ext,
        None => return Err(anyhow!("expected URL {:?} path {:?} to have a file extension", file, path)),
      };
      let tmpfile = tmpdir.path().join(format!("{i}.{}", ext.to_str().unwrap()));
      match Self::fetch_url_into_file(&request_client, file, &tmpfile) {
        Ok(body) => {
          eprintln!("body is {} bytes", body);
          let _ = fs::copy(&tmpfile, "/tmp/file");
        }
        Err(e) => return Err(anyhow!("error fetching {} : {}", file, e)),
      };

      if !inscription.contains_key("utxo") {
        return Err(anyhow!("expected `inscription` to contain `utxo`"));
      }
      let utxo = inscription.get("utxo").unwrap();
      if !utxo.is_string() {
        return Err(anyhow!("expected `inscriptions[].utxo` to be a string, not {:?}", utxo));
      }
      let utxo = utxo.as_str().unwrap();
      let utxo = match OutPoint::from_str(utxo) {
        Ok(utxo) => utxo,
        _ => return Err(anyhow!("expected `inscriptions[].utxo` to be a valid utxo, not {:?}", utxo)),
      };

      let metadata = if inscription.contains_key("metadata") {
        Some(inscription.get("metadata").unwrap().clone())
      } else {
        None
      };

      if !inscription.contains_key("destination") {
        return Err(anyhow!("expected `inscription` to contain `destination`"));
      }
      let destination = inscription.get("destination").unwrap();
      if !destination.is_string() {
        return Err(anyhow!("expected `inscriptions[].destination` to be a string, not {:?}", destination));
      }
      let destination = destination.as_str().unwrap();
      let destination: Address<NetworkUnchecked> = match destination.parse() {
        Ok(destination) => destination,
        Err(_) => return Err(anyhow!("expected `inscriptions[].destination` to be a valid address, not {:?}", destination)),
      };

      /* we don't need to check addresses for the correct network type here, because the batch file expects unchecked addresses
         let destination: Address = match destination.clone().require_network(chain.network()) {
           Ok(destination) => destination,
           Err(_) => return Err(anyhow!("expected `inscriptions[].destination` to be valid for the current chain, not {:?}", destination)),
         };
       */

      entries.push(BatchEntry {
        delegate: None,
        destination: Some(destination),
        file: tmpfile.into(),
        metadata: None,
        metadata_json: metadata,
        metaprotocol: None,
        utxo: Some(utxo),
      });
    }

    let mut fees = Vec::new();

    for fees_utxo in fees_utxos {
      if !fees_utxo.is_string() {
        return Err(anyhow!("expected `fees_utxos` to only contain strings, not {:?}", fees_utxo));
      }

      let fees_utxo = fees_utxo.as_str().unwrap();
      let fees_utxo = match OutPoint::from_str(fees_utxo) {
        Ok(fees_utxo) => fees_utxo,
        _ => return Err(anyhow!("expected `fees_utxos` to contain valid utxos, not {:?}", fees_utxo)),
      };

      fees.push(fees_utxo);
    }

    let batchfile = Batchfile {
      fees: Some(fees),
      inscriptions: entries,
      mode: Mode::SeparateOutputs,
      parent,
      ..Default::default()
    };

    let mut utxos = BTreeMap::new();
    let locked_utxos = BTreeSet::new();
    let runic_utxos = BTreeSet::new();
    let client = index.client();

    let change = None;

    let postage;
    let destinations;
    let fee_utxos;
    let inscribe_on_specific_utxos;
    let inscriptions;
    let mode;
    let parent_info;
    let next_inscriptions;

    let compress = false;

        parent_info = Inscribe::get_parent_info(batchfile.parent, &index, &utxos, &client, chain, batchfile.parent_satpoint, no_wallet, None)?;

        postage = batchfile
          .postage
          .map(Amount::from_sat)
          .unwrap_or(TARGET_POSTAGE);

        (inscriptions, destinations, inscribe_on_specific_utxos, fee_utxos) = batchfile.inscriptions(
          &client,
          chain,
          parent_info.as_ref().map(|info| info.tx_out.value),
          None,
          Amount::from_sat(0),
          compress,
          &mut utxos,
        )?;
        next_inscriptions = Vec::new();

        mode = batchfile.mode;

        if batchfile.sat.is_some() && mode != Mode::SameSat {
          return Err(anyhow!("`sat` can only be set in `same-sat` mode"));
        }

    let satpoint = None;

    let key = Some(Self::get_temporary_key(index, chain)?);
    key.clone().map(|key| eprintln!("using key {key}"));

    let reveal_psbt = if data.contains_key("reveal_psbt") {
      let reveal_psbt = data.get("reveal_psbt").unwrap();
      if !reveal_psbt.is_string() {
        return Err(anyhow!("expected `reveal_psbt` to be a string, not {:?}", reveal_psbt));
      }
      let reveal_psbt = reveal_psbt.as_str().unwrap();
      eprintln!("got reveal_psbt: {reveal_psbt}");
      match Psbt::from_str(reveal_psbt) {
        Ok(psbt) => Some(psbt),
        Err(e) => return Err(anyhow!("reveal_psbt {}", e)),
      }
    } else {
      None
    };

    Batch {
      commit_fee_rate: FeeRate::try_from(0.0).unwrap(),
      commit_only: false,
      commit_vsize,
      commitment: None,
      commitment_output: None,
      destinations,
      dump: true,
      dry_run: false,
      fee_utxos,
      inscribe_on_specific_utxos,
      inscriptions,
      key,
      mode,
      next_inscriptions,
      no_backup: true,
      no_broadcast: true,
      no_limit: false,
      no_wallet,
      parent_info,
      postage,
      reinscribe: false,
      reveal_fee: None,
      reveal_fee_rate: FeeRate::try_from(0.0).unwrap(),
      reveal_input: Vec::new(),
      reveal_psbt,
      satpoint,
    }
    .inscribe(chain, &index, &client, &locked_utxos, runic_utxos, &mut utxos, Vec::new(), change)
  }
}

#[cfg(test)]
mod tests {
  use {
    self::batch::BatchEntry,
    super::*,
    serde_yaml::{Mapping, Value},
  };

  #[test]
  fn reveal_transaction_pays_fee() {
    let utxos = vec![(outpoint(1), Amount::from_sat(20000))];
    let inscription = inscription("text/plain", "ord");
    let commit_address = change(0);
    let reveal_address = recipient();
    let change = [commit_address, change(1)];

    let (commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint: Some(satpoint(1, 0)),
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(1.0).unwrap(),
      reveal_fee_rate: FeeRate::try_from(1.0).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      BTreeMap::new(),
      Chain::Mainnet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      change,
    )
    .unwrap();

    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    let fee = Amount::from_sat((1.0 * (reveal_tx.vsize() as f64)).ceil() as u64);

    assert_eq!(
      reveal_tx.output[0].value,
      20000 - fee.to_sat() - (20000 - commit_tx.output[0].value),
    );
  }

  #[test]
  fn inscribe_transactions_opt_in_to_rbf() {
    let utxos = vec![(outpoint(1), Amount::from_sat(20000))];
    let inscription = inscription("text/plain", "ord");
    let commit_address = change(0);
    let reveal_address = recipient();
    let change = [commit_address, change(1)];

    let (commit_tx, reveal_tx, _, _) = Batch {
      satpoint: Some(satpoint(1, 0)),
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(1.0).unwrap(),
      reveal_fee_rate: FeeRate::try_from(1.0).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      BTreeMap::new(),
      Chain::Mainnet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      change,
    )
    .unwrap();

    assert!(commit_tx.is_explicitly_rbf());
    assert!(reveal_tx.is_explicitly_rbf());
  }

  #[test]
  fn inscribe_with_no_satpoint_and_no_cardinal_utxos() {
    let utxos = vec![(outpoint(1), Amount::from_sat(1000))];
    let mut inscriptions = BTreeMap::new();
    inscriptions.insert(
      SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      inscription_id(1),
    );

    let inscription = inscription("text/plain", "ord");
    let satpoint = None;
    let commit_address = change(0);
    let reveal_address = recipient();

    let error = Batch {
      satpoint,
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(1.0).unwrap(),
      reveal_fee_rate: FeeRate::try_from(1.0).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      inscriptions,
      Chain::Mainnet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(1)],
    )
    .unwrap_err()
    .to_string();

    assert!(
      error.contains("wallet contains no cardinal utxos"),
      "{}",
      error
    );
  }

  #[test]
  fn inscribe_with_no_satpoint_and_enough_cardinal_utxos() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(20_000)),
      (outpoint(2), Amount::from_sat(20_000)),
    ];
    let mut inscriptions = BTreeMap::new();
    inscriptions.insert(
      SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      inscription_id(1),
    );

    let inscription = inscription("text/plain", "ord");
    let satpoint = None;
    let commit_address = change(0);
    let reveal_address = recipient();

    assert!(Batch {
      satpoint,
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(1.0).unwrap(),
      reveal_fee_rate: FeeRate::try_from(1.0).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      inscriptions,
      Chain::Mainnet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(1)],
    )
    .is_ok())
  }

  #[test]
  fn inscribe_with_custom_fee_rate() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(20_000)),
    ];
    let mut inscriptions = BTreeMap::new();
    inscriptions.insert(
      SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      inscription_id(1),
    );

    let inscription = inscription("text/plain", "ord");
    let satpoint = None;
    let commit_address = change(0);
    let reveal_address = recipient();
    let fee_rate = 3.3;

    let (commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint,
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(fee_rate).unwrap(),
      reveal_fee_rate: FeeRate::try_from(fee_rate).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(1)],
    )
    .unwrap();

    let sig_vbytes = 17;
    let fee = FeeRate::try_from(fee_rate)
      .unwrap()
      .fee(commit_tx.vsize() + sig_vbytes)
      .to_sat();

    let reveal_value = commit_tx
      .output
      .iter()
      .map(|o| o.value)
      .reduce(|acc, i| acc + i)
      .unwrap();

    assert_eq!(reveal_value, 20_000 - fee);

    let fee = FeeRate::try_from(fee_rate)
      .unwrap()
      .fee(reveal_tx.vsize())
      .to_sat();

    assert_eq!(
      reveal_tx.output[0].value,
      20_000 - fee - (20_000 - commit_tx.output[0].value),
    );
  }

  #[test]
  fn inscribe_with_parent() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(20_000)),
    ];

    let mut inscriptions = BTreeMap::new();
    let parent_inscription = inscription_id(1);
    let parent_info = ParentInfo {
      destination: change(3),
      id: parent_inscription,
      location: SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      tx_out: TxOut {
        script_pubkey: change(0).script_pubkey(),
        value: 10000,
      },
    };

    inscriptions.insert(parent_info.location, parent_inscription);

    let child_inscription = InscriptionTemplate {
      parent: Some(parent_inscription),
      ..Default::default()
    }
    .into();

    let commit_address = change(1);
    let reveal_address = recipient();
    let fee_rate = 4.0;

    let (commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint: None,
      parent_info: Some(parent_info.clone()),
      inscriptions: vec![child_inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(fee_rate).unwrap(),
      reveal_fee_rate: FeeRate::try_from(fee_rate).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    )
    .unwrap();

    let sig_vbytes = 17;
    let fee = FeeRate::try_from(fee_rate)
      .unwrap()
      .fee(commit_tx.vsize() + sig_vbytes)
      .to_sat();

    let reveal_value = commit_tx
      .output
      .iter()
      .map(|o| o.value)
      .reduce(|acc, i| acc + i)
      .unwrap();

    assert_eq!(reveal_value, 20_000 - fee);

    let sig_vbytes = 16;
    let fee = FeeRate::try_from(fee_rate)
      .unwrap()
      .fee(reveal_tx.vsize() + sig_vbytes)
      .to_sat();

    assert_eq!(fee, commit_tx.output[0].value - reveal_tx.output[1].value,);
    assert_eq!(
      reveal_tx.output[0].script_pubkey,
      parent_info.destination.script_pubkey()
    );
    assert_eq!(reveal_tx.output[0].value, parent_info.tx_out.value);
    pretty_assert_eq!(
      reveal_tx.input[0],
      TxIn {
        previous_output: parent_info.location.outpoint,
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        ..Default::default()
      }
    );
  }

  #[test]
  fn inscribe_with_commit_fee_rate() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(20_000)),
    ];
    let mut inscriptions = BTreeMap::new();
    inscriptions.insert(
      SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      inscription_id(1),
    );

    let inscription = inscription("text/plain", "ord");
    let satpoint = None;
    let commit_address = change(0);
    let reveal_address = recipient();
    let commit_fee_rate = 3.3;
    let fee_rate = 1.0;

    let (commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint,
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(commit_fee_rate).unwrap(),
      reveal_fee_rate: FeeRate::try_from(fee_rate).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(1)],
    )
    .unwrap();

    let sig_vbytes = 17;
    let fee = FeeRate::try_from(commit_fee_rate)
      .unwrap()
      .fee(commit_tx.vsize() + sig_vbytes)
      .to_sat();

    let reveal_value = commit_tx
      .output
      .iter()
      .map(|o| o.value)
      .reduce(|acc, i| acc + i)
      .unwrap();

    assert_eq!(reveal_value, 20_000 - fee);

    let fee = FeeRate::try_from(fee_rate)
      .unwrap()
      .fee(reveal_tx.vsize())
      .to_sat();

    assert_eq!(
      reveal_tx.output[0].value,
      20_000 - fee - (20_000 - commit_tx.output[0].value),
    );
  }

  #[test]
  fn inscribe_over_max_standard_tx_weight() {
    let utxos = vec![(outpoint(1), Amount::from_sat(50 * COIN_VALUE))];

    let inscription = inscription("text/plain", [0; MAX_STANDARD_TX_WEIGHT as usize]);
    let satpoint = None;
    let commit_address = change(0);
    let reveal_address = recipient();

    let error = Batch {
      satpoint,
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(1.0).unwrap(),
      reveal_fee_rate: FeeRate::try_from(1.0).unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      BTreeMap::new(),
      Chain::Mainnet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(1)],
    )
    .unwrap_err()
    .to_string();

    assert!(
      error.contains(&format!("reveal transaction weight greater than {MAX_STANDARD_TX_WEIGHT} (MAX_STANDARD_TX_WEIGHT): 402799")),
      "{}",
      error
    );
  }

  #[test]
  fn inscribe_with_no_max_standard_tx_weight() {
    let utxos = vec![(outpoint(1), Amount::from_sat(50 * COIN_VALUE))];

    let inscription = inscription("text/plain", [0; MAX_STANDARD_TX_WEIGHT as usize]);
    let satpoint = None;
    let commit_address = change(0);
    let reveal_address = recipient();

    let (_commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint,
      parent_info: None,
      inscriptions: vec![inscription],
      destinations: vec![reveal_address],
      commit_fee_rate: FeeRate::try_from(1.0).unwrap(),
      reveal_fee_rate: FeeRate::try_from(1.0).unwrap(),
      no_limit: true,
      reinscribe: false,
      postage: TARGET_POSTAGE,
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      BTreeMap::new(),
      Chain::Mainnet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(1)],
    )
    .unwrap();

    assert!(reveal_tx.size() >= MAX_STANDARD_TX_WEIGHT as usize);
  }

  #[test]
  fn cbor_and_json_metadata_flags_conflict() {
    assert_regex_match!(
      Arguments::try_parse_from([
        "ord",
        "wallet",
        "inscribe",
        "--cbor-metadata",
        "foo",
        "--json-metadata",
        "bar",
        "--file",
        "baz",
      ])
      .unwrap_err()
      .to_string(),
      ".*--cbor-metadata.*cannot be used with.*--json-metadata.*"
    );
  }

  #[test]
  fn batch_is_loaded_from_yaml_file() {
    let parent = "8d363b28528b0cb86b5fd48615493fb175bdf132d2a3d20b4251bba3f130a5abi0"
      .parse::<InscriptionId>()
      .unwrap();

    let tempdir = TempDir::new().unwrap();

    let inscription_path = tempdir.path().join("tulip.txt");
    fs::write(&inscription_path, "tulips are pretty").unwrap();

    let brc20_path = tempdir.path().join("token.json");

    let batch_path = tempdir.path().join("batch.yaml");
    fs::write(
      &batch_path,
      format!(
        "mode: separate-outputs
parent: {parent}
inscriptions:
- file: {}
  metadata:
    title: Lorem Ipsum
    description: Lorem ipsum dolor sit amet, consectetur adipiscing elit. In tristique, massa nec condimentum venenatis, ante massa tempor velit, et accumsan ipsum ligula a massa. Nunc quis orci ante.
- file: {}
  metaprotocol: brc-20
",
        inscription_path.display(),
        brc20_path.display()
      ),
    )
    .unwrap();

    let mut metadata = Mapping::new();
    metadata.insert(
      Value::String("title".to_string()),
      Value::String("Lorem Ipsum".to_string()),
    );
    metadata.insert(Value::String("description".to_string()), Value::String("Lorem ipsum dolor sit amet, consectetur adipiscing elit. In tristique, massa nec condimentum venenatis, ante massa tempor velit, et accumsan ipsum ligula a massa. Nunc quis orci ante.".to_string()));

    assert_eq!(
      Batchfile::load(&batch_path).unwrap(),
      Batchfile {
        inscriptions: vec![
          BatchEntry {
            file: inscription_path,
            metadata: Some(Value::Mapping(metadata)),
            ..Default::default()
          },
          BatchEntry {
            file: brc20_path,
            metaprotocol: Some("brc-20".to_string()),
            ..Default::default()
          }
        ],
        parent: Some(parent),
        ..Default::default()
      }
    );
  }

  #[test]
  fn batch_with_unknown_field_throws_error() {
    let tempdir = TempDir::new().unwrap();
    let batch_path = tempdir.path().join("batch.yaml");
    fs::write(
      &batch_path,
      "mode: shared-output\ninscriptions:\n- file: meow.wav\nunknown: 1.)what",
    )
    .unwrap();

    assert!(Batchfile::load(&batch_path)
      .unwrap_err()
      .to_string()
      .contains("unknown field `unknown`"));
  }

  #[test]
  fn batch_inscribe_with_parent() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(50_000)),
    ];

    let parent = inscription_id(1);

    let parent_info = ParentInfo {
      destination: change(3),
      id: parent,
      location: SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      tx_out: TxOut {
        script_pubkey: change(0).script_pubkey(),
        value: 10000,
      },
    };

    let mut wallet_inscriptions = BTreeMap::new();
    wallet_inscriptions.insert(parent_info.location, parent);

    let commit_address = change(1);
    let reveal_addresses = vec![recipient()];

    let inscriptions = vec![
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
    ];

    let mode = Mode::SharedOutput;

    let fee_rate = 4.0.try_into().unwrap();

    let (commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint: None,
      parent_info: Some(parent_info.clone()),
      inscriptions,
      destinations: reveal_addresses,
      commit_fee_rate: fee_rate,
      reveal_fee_rate: fee_rate,
      no_limit: false,
      reinscribe: false,
      postage: Amount::from_sat(10_000),
      mode,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      wallet_inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    )
    .unwrap();

    let sig_vbytes = 17;
    let fee = fee_rate.fee(commit_tx.vsize() + sig_vbytes).to_sat();

    let reveal_value = commit_tx
      .output
      .iter()
      .map(|o| o.value)
      .reduce(|acc, i| acc + i)
      .unwrap();

    assert_eq!(reveal_value, 50_000 - fee);

    let sig_vbytes = 16;
    let fee = fee_rate.fee(reveal_tx.vsize() + sig_vbytes).to_sat();

    assert_eq!(fee, commit_tx.output[0].value - reveal_tx.output[1].value,);
    assert_eq!(
      reveal_tx.output[0].script_pubkey,
      parent_info.destination.script_pubkey()
    );
    assert_eq!(reveal_tx.output[0].value, parent_info.tx_out.value);
    pretty_assert_eq!(
      reveal_tx.input[0],
      TxIn {
        previous_output: parent_info.location.outpoint,
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        ..Default::default()
      }
    );
  }

  #[test]
  fn batch_inscribe_with_parent_not_enough_cardinals_utxos_fails() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(20_000)),
    ];

    let parent = inscription_id(1);

    let parent_info = ParentInfo {
      destination: change(3),
      id: parent,
      location: SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      tx_out: TxOut {
        script_pubkey: change(0).script_pubkey(),
        value: 10000,
      },
    };

    let mut wallet_inscriptions = BTreeMap::new();
    wallet_inscriptions.insert(parent_info.location, parent);

    let inscriptions = vec![
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
    ];

    let commit_address = change(1);
    let reveal_addresses = vec![recipient()];

    let error = Batch {
      satpoint: None,
      parent_info: Some(parent_info.clone()),
      inscriptions,
      destinations: reveal_addresses,
      commit_fee_rate: 4.0.try_into().unwrap(),
      reveal_fee_rate: 4.0.try_into().unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: Amount::from_sat(10_000),
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      wallet_inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains(
      "wallet does not contain enough cardinal UTXOs, please add additional funds to wallet."
    ));
  }

  #[test]
  #[should_panic(
    expected = "invariant: destination addresses and number of inscriptions doesn't match"
  )]
  fn batch_inscribe_with_inconsistent_reveal_addresses_panics() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(80_000)),
    ];

    let parent = inscription_id(1);

    let parent_info = ParentInfo {
      destination: change(3),
      id: parent,
      location: SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      tx_out: TxOut {
        script_pubkey: change(0).script_pubkey(),
        value: 10000,
      },
    };

    let mut wallet_inscriptions = BTreeMap::new();
    wallet_inscriptions.insert(parent_info.location, parent);

    let inscriptions = vec![
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
    ];

    let commit_address = change(1);
    let reveal_addresses = vec![recipient(), recipient()];

    let _ = Batch {
      satpoint: None,
      parent_info: Some(parent_info.clone()),
      inscriptions,
      destinations: reveal_addresses,
      commit_fee_rate: 4.0.try_into().unwrap(),
      reveal_fee_rate: 4.0.try_into().unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: Amount::from_sat(10_000),
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      wallet_inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    );
  }

  #[test]
  fn batch_inscribe_over_max_standard_tx_weight() {
    let utxos = vec![(outpoint(1), Amount::from_sat(50 * COIN_VALUE))];

    let wallet_inscriptions = BTreeMap::new();

    let inscriptions = vec![
      inscription("text/plain", [0; MAX_STANDARD_TX_WEIGHT as usize / 3]),
      inscription("text/plain", [0; MAX_STANDARD_TX_WEIGHT as usize / 3]),
      inscription("text/plain", [0; MAX_STANDARD_TX_WEIGHT as usize / 3]),
    ];

    let commit_address = change(1);
    let reveal_addresses = vec![recipient()];

    let error = Batch {
      satpoint: None,
      parent_info: None,
      inscriptions,
      destinations: reveal_addresses,
      commit_fee_rate: 1.0.try_into().unwrap(),
      reveal_fee_rate: 1.0.try_into().unwrap(),
      no_limit: false,
      reinscribe: false,
      postage: Amount::from_sat(30_000),
      mode: Mode::SharedOutput,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      wallet_inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    )
    .unwrap_err()
    .to_string();

    assert!(
      error.contains(&format!("reveal transaction weight greater than {MAX_STANDARD_TX_WEIGHT} (MAX_STANDARD_TX_WEIGHT): 402841")),
      "{}",
      error
    );
  }

  #[test]
  fn batch_inscribe_into_separate_outputs() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(80_000)),
    ];

    let wallet_inscriptions = BTreeMap::new();

    let commit_address = change(1);
    let reveal_addresses = vec![recipient(), recipient(), recipient()];

    let inscriptions = vec![
      inscription("text/plain", [b'O'; 100]),
      inscription("text/plain", [b'O'; 111]),
      inscription("text/plain", [b'O'; 222]),
    ];

    let mode = Mode::SeparateOutputs;

    let fee_rate = 4.0.try_into().unwrap();

    let (_commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint: None,
      parent_info: None,
      inscriptions,
      destinations: reveal_addresses,
      commit_fee_rate: fee_rate,
      reveal_fee_rate: fee_rate,
      no_limit: false,
      reinscribe: false,
      postage: Amount::from_sat(10_000),
      mode,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      wallet_inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    )
    .unwrap();

    assert_eq!(reveal_tx.output.len(), 3);
    assert!(reveal_tx
      .output
      .iter()
      .all(|output| output.value == TARGET_POSTAGE.to_sat()));
  }

  #[test]
  fn batch_inscribe_into_separate_outputs_with_parent() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(50_000)),
    ];

    let parent = inscription_id(1);

    let parent_info = ParentInfo {
      destination: change(3),
      id: parent,
      location: SatPoint {
        outpoint: outpoint(1),
        offset: 0,
      },
      tx_out: TxOut {
        script_pubkey: change(0).script_pubkey(),
        value: 10000,
      },
    };

    let mut wallet_inscriptions = BTreeMap::new();
    wallet_inscriptions.insert(parent_info.location, parent);

    let commit_address = change(1);
    let reveal_addresses = vec![recipient(), recipient(), recipient()];

    let inscriptions = vec![
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
      InscriptionTemplate {
        parent: Some(parent),
        ..Default::default()
      }
      .into(),
    ];

    let mode = Mode::SeparateOutputs;

    let fee_rate = 4.0.try_into().unwrap();

    let (commit_tx, reveal_tx, _private_key, _) = Batch {
      satpoint: None,
      parent_info: Some(parent_info.clone()),
      inscriptions,
      destinations: reveal_addresses,
      commit_fee_rate: fee_rate,
      reveal_fee_rate: fee_rate,
      no_limit: false,
      reinscribe: false,
      postage: Amount::from_sat(10_000),
      mode,
      ..Default::default()
    }
    .create_batch_inscription_transactions(
      wallet_inscriptions,
      Chain::Signet,
      BTreeSet::new(),
      BTreeSet::new(),
      utxos.into_iter().collect(),
      [commit_address, change(2)],
    )
    .unwrap();

    assert_eq!(
      parent,
      ParsedEnvelope::from_transaction(&reveal_tx)[0]
        .payload
        .parent()
        .unwrap()
    );
    assert_eq!(
      parent,
      ParsedEnvelope::from_transaction(&reveal_tx)[1]
        .payload
        .parent()
        .unwrap()
    );

    let sig_vbytes = 17;
    let fee = fee_rate.fee(commit_tx.vsize() + sig_vbytes).to_sat();

    let reveal_value = commit_tx
      .output
      .iter()
      .map(|o| o.value)
      .reduce(|acc, i| acc + i)
      .unwrap();

    assert_eq!(reveal_value, 50_000 - fee);

    assert_eq!(
      reveal_tx.output[0].script_pubkey,
      parent_info.destination.script_pubkey()
    );
    assert_eq!(reveal_tx.output[0].value, parent_info.tx_out.value);
    pretty_assert_eq!(
      reveal_tx.input[0],
      TxIn {
        previous_output: parent_info.location.outpoint,
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        ..Default::default()
      }
    );
  }

  #[test]
  fn example_batchfile_deserializes_successfully() {
    Batchfile::load(Path::new("batch.yaml")).unwrap();
  }

  #[test]
  fn flags_conflict_with_batch() {
    for (flag, value) in [
      ("--file", Some("foo")),
      (
        "--destination",
        Some("tb1qsgx55dp6gn53tsmyjjv4c2ye403hgxynxs0dnm"),
      ),
      ("--cbor-metadata", Some("foo")),
      ("--json-metadata", Some("foo")),
      (
        "--satpoint",
        Some("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:0:0"),
      ),
      ("--reinscribe", None),
      ("--metaprotocol", Some("foo")),
      (
        "--parent",
        Some("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33bi0"),
      ),
    ] {
      let mut args = vec![
        "ord",
        "wallet",
        "inscribe",
        "--fee-rate",
        "1",
        "--batch",
        "foo.yaml",
        flag,
      ];

      if let Some(value) = value {
        args.push(value);
      }

      assert!(Arguments::try_parse_from(args)
        .unwrap_err()
        .to_string()
        .contains("the argument '--batch <BATCH>' cannot be used with"));
    }
  }

  #[test]
  fn batch_or_file_is_required() {
    assert!(
      Arguments::try_parse_from(["ord", "wallet", "inscribe", "--fee-rate", "1",])
        .unwrap_err()
        .to_string()
        .contains("error: the following required arguments were not provided:\n  <--file <FILE>|--batch <BATCH>>")
    );
  }

  #[test]
  fn satpoint_and_sat_flags_conflict() {
    assert_regex_match!(
      Arguments::try_parse_from([
        "ord",
        "--index-sats",
        "wallet",
        "inscribe",
        "--sat",
        "50000000000",
        "--satpoint",
        "038112028c55f3f77cc0b8b413df51f70675f66be443212da0642b7636f68a00:1:0",
        "--file",
        "baz",
      ])
      .unwrap_err()
      .to_string(),
      ".*--sat.*cannot be used with.*--satpoint.*"
    );
  }
}
