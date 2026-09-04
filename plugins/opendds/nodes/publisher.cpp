// dds-publisher — a wk node that publishes on a DDS topic.
//
//   dds-publisher --peer <node-name> [--count N] [--forever]
//
// Below the argument parsing and the pump, this is ordinary OpenDDS: create a
// participant, register a type, create a topic, a publisher and a DataWriter,
// then write samples. Nothing about it is wasm-specific, which is the point.
// Everything the port had to arrange lives in wk_dds_node.h and in the
// libraries underneath.

#include "wk_dds_node.h"
#include "WkMessageTypeSupportImpl.h"

#include <dds/DCPS/Marked_Default_Qos.h>

#include <cstdio>

int ACE_TMAIN(int argc, ACE_TCHAR* argv[])
{
  wk_dds::Options opt;
  if (!wk_dds::parse(argc, argv, opt)) {
    std::fprintf(stderr,
      "usage: dds-publisher --peer <node-name> [--self <addr>] [--count N] [--forever]\n");
    return 1;
  }

  DDS::DomainParticipant_var dp = wk_dds::start(argc, argv, opt);
  if (!dp) return 1;

  wk::MessageTypeSupport_var ts = new wk::MessageTypeSupportImpl;
  if (ts->register_type(dp, "") != DDS::RETCODE_OK) {
    std::fprintf(stderr, "dds-publisher: register_type failed\n");
    return 1;
  }

  CORBA::String_var type_name = ts->get_type_name();
  DDS::Topic_var topic = dp->create_topic("wk Messages", type_name,
                                          TOPIC_QOS_DEFAULT, 0,
                                          OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!topic) {
    std::fprintf(stderr, "dds-publisher: create_topic failed\n");
    return 1;
  }

  DDS::Publisher_var pub = dp->create_publisher(PUBLISHER_QOS_DEFAULT, 0,
                                                OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!pub) {
    std::fprintf(stderr, "dds-publisher: create_publisher failed\n");
    return 1;
  }

  // RELIABLE, and this is worth being explicit about rather than taking the
  // default: it is what makes the writer retransmit on ACKNACK, which is the
  // machinery that runs on the reactor and therefore on the pump. A demo that
  // used BEST_EFFORT would prove much less.
  DDS::DataWriterQos qos;
  pub->get_default_datawriter_qos(qos);
  qos.reliability.kind = DDS::RELIABLE_RELIABILITY_QOS;
  qos.history.kind = DDS::KEEP_ALL_HISTORY_QOS;

  DDS::DataWriter_var writer = pub->create_datawriter(topic, qos, 0,
                                                      OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!writer) {
    std::fprintf(stderr, "dds-publisher: create_datawriter failed\n");
    return 1;
  }

  wk::MessageDataWriter_var mw = wk::MessageDataWriter::_narrow(writer);
  if (!mw) {
    std::fprintf(stderr, "dds-publisher: narrow failed\n");
    return 1;
  }

  // Wait for a reader, using the DDS API's own status condition. The wait
  // pumps -- that is what advances discovery underneath it — so this loop is
  // both the application's "wait for a subscriber" and the middleware's
  // "make progress". See wk_dds_node.h.
  std::fprintf(stderr, "dds-publisher: waiting for a subscriber...\n");
  DDS::StatusCondition_var cond = writer->get_statuscondition();
  cond->set_enabled_statuses(DDS::PUBLICATION_MATCHED_STATUS);
  DDS::WaitSet_var ws = new DDS::WaitSet;
  ws->attach_condition(cond);

  for (;;) {
    DDS::PublicationMatchedStatus matched;
    if (writer->get_publication_matched_status(matched) != DDS::RETCODE_OK) {
      std::fprintf(stderr, "dds-publisher: get_publication_matched_status failed\n");
      return 1;
    }
    if (matched.current_count >= 1) break;

    DDS::ConditionSeq active;
    const DDS::Duration_t forever = { DDS::DURATION_INFINITE_SEC,
                                      DDS::DURATION_INFINITE_NSEC };
    if (ws->wait(active, forever) != DDS::RETCODE_OK) {
      std::fprintf(stderr, "dds-publisher: wait failed\n");
      return 1;
    }
  }
  ws->detach_condition(cond);
  std::fprintf(stderr, "dds-publisher: subscriber found, publishing\n");

  wk::Message msg;
  msg.id = 1;
  msg.from = "dds-publisher";
  msg.text = "hello from a wasm node";
  msg.count = 0;

  const DDS::InstanceHandle_t handle = mw->register_instance(msg);

  for (int i = 0; opt.forever || i < opt.count; ++i) {
    msg.count = i;
    const DDS::ReturnCode_t rc = mw->write(msg, handle);
    if (rc != DDS::RETCODE_OK) {
      std::fprintf(stderr, "dds-publisher: write returned %d\n", (int)rc);
      break;
    }
    std::printf("sent  #%d\n", i);
    std::fflush(stdout);

    // One sample a second, and the wait is a PUMP, not a sleep: the reactor
    // has retransmissions and heartbeats to send between samples, and
    // ACE_OS::sleep would stop the participant dead for the whole second.
    wk_dds::pump(ACE_Time_Value(1, 0));
  }

  // Let the reliable writer finish before tearing the participant down,
  // otherwise the last samples are dropped on the floor at shutdown.
  DDS::Duration_t ack_timeout = { 10, 0 };
  mw->wait_for_acknowledgments(ack_timeout);

  std::fprintf(stderr, "dds-publisher: done\n");
  dp->delete_contained_entities();
  TheParticipantFactory->delete_participant(dp);
  TheServiceParticipant->shutdown();
  return 0;
}
