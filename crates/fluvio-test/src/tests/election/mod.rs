use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::stream::StreamExt;

use fluvio::{Offset, RecordKey};
use fluvio_controlplane_metadata::partition::PartitionSpec;
use fluvio_future::timer::sleep;
use structopt::StructOpt;

use fluvio_test_derive::fluvio_test;
use fluvio_test_util::test_meta::derive_attr::TestRequirements;
use fluvio_test_util::test_meta::environment::EnvironmentSetup;
use fluvio_test_util::test_meta::{TestOption, TestCase};
use fluvio_test_util::test_meta::test_result::TestResult;
use fluvio_test_util::test_runner::test_driver::{TestDriver};
use fluvio_test_util::test_runner::test_meta::FluvioTestMeta;

// time to wait for ac
const ACK_WAIT: u64 = 10;

#[derive(Debug, Clone)]
pub struct ElectionTestCase {
    pub environment: EnvironmentSetup,
    pub option: ElectionTestOption,
}

impl From<TestCase> for ElectionTestCase {
    fn from(test_case: TestCase) -> Self {
        let election_option = test_case
            .option
            .as_any()
            .downcast_ref::<ElectionTestOption>()
            .expect("ElectionTestOption")
            .to_owned();
        Self {
            environment: test_case.environment,
            option: election_option,
        }
    }
}

#[derive(Debug, Clone, StructOpt, Default, PartialEq)]
#[structopt(name = "Fluvio ELECTION Test")]
pub struct ElectionTestOption {}

impl TestOption for ElectionTestOption {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[fluvio_test(topic = "test")]
pub async fn election(
    mut test_driver: Arc<FluvioTestDriver>,
    mut test_case: TestCase,
) -> TestResult {
    println!("Starting election test");

    // first a create simple message
    let topic_name = test_case.environment.topic_name();
    let producer = test_driver.create_producer(&topic_name).await;

    producer
        .send(RecordKey::NULL, "msg1")
        .await
        .expect("sending");

    // this is hack now, because we don't have ack
    sleep(Duration::from_secs(ACK_WAIT)).await;

    let admin = test_driver.client().admin().await;

    let partitions = admin
        .list::<PartitionSpec, _>(vec![])
        .await
        .expect("partitions");

    assert_eq!(partitions.len(), 1);
    let test_topic = &partitions[0];
    let status = &test_topic.status;
    let leader = &status.leader;
    assert_eq!(leader.leo, 1);
    assert_eq!(leader.hw, 1);
    let follower_status = &status.replicas[0];
    assert_eq!(follower_status.hw, 1);
    assert_eq!(follower_status.leo, 1);
    let follower_id = follower_status.spu;

    // find leader spu
    let leader = test_topic.spec.leader;
    println!("leader was: {}", leader);

    println!("terminating leader and waiting for election..");

    let cluster_manager = test_driver
        .get_cluster()
        .expect("cluster")
        .env_driver()
        .create_cluster_manager();

    cluster_manager.terminate_spu(leader).expect("terminate");

    sleep(Duration::from_secs(ACK_WAIT)).await;

    println!("checking for new leader");

    let partition_status2 = admin
        .list::<PartitionSpec, _>(vec![])
        .await
        .expect("partitions");

    let status2 = &partition_status2[0];
    assert_eq!(status2.spec.leader, follower_id); // switch leader to follower

    // create new producer
    let producer2 = test_driver.create_producer(&topic_name).await;

    producer2
        .send(RecordKey::NULL, "msg2")
        .await
        .expect("sending");

    // wait until this gets written
    sleep(Duration::from_secs(ACK_WAIT)).await;

    {
        let partition_status = admin
            .list::<PartitionSpec, _>(vec![])
            .await
            .expect("partitions");
        let leader_status = &partition_status[0].status.leader;
        assert_eq!(leader_status.leo, 2);
        assert_eq!(leader_status.hw, 1);
    }

    // start previous follower
    println!("starting leader again: {}", &leader);
    let leader_spu = cluster_manager.create_spu_absolute(leader as u16);
    leader_spu.start().expect("start");

    // wait until prev leader has caught up
    sleep(Duration::from_secs(ACK_WAIT)).await;

    println!("checking that prev leader has fully caught up");
    {
        let partition_status = admin
            .list::<PartitionSpec, _>(vec![])
            .await
            .expect("partitions");
        let leader_status = &partition_status[0].status.leader;
        assert_eq!(leader_status.leo, 2);
        assert_eq!(leader_status.hw, 2);
    }

    println!("terminating current leader");
    cluster_manager
        .terminate_spu(follower_id)
        .expect("terminate");

    sleep(Duration::from_secs(ACK_WAIT)).await;

    println!("checking leader again");

    {
        let partition_status = admin
            .list::<PartitionSpec, _>(vec![])
            .await
            .expect("partitions");
        let leader_status = &partition_status[0];
        assert_eq!(leader_status.spec.leader, leader);
    }

    let consumer = test_driver.get_consumer(&topic_name).await;
    let mut stream = consumer
        .stream(Offset::absolute(0).expect("offset"))
        .await
        .expect("stream");

    println!("checking msg1");
    let records = stream.next().await.expect("get next").expect("next");
    assert_eq!(records.value(), "msg1".as_bytes());

    println!("checking msg2");
    let records = stream.next().await.expect("get next").expect("next");
    assert_eq!(records.value(), "msg2".as_bytes());
}