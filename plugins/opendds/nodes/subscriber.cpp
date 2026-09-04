// dds-subscriber — a wk node that subscribes to a DDS topic.
//
//   dds-subscriber --peer <node-name> [--count N] [--forever]
//
// The mirror of publisher.cpp, and equally ordinary DDS: participant, type,
// topic, subscriber, DataReader, then read what arrives. It uses a WaitSet
// rather than a listener deliberately — a listener would be called from the
// middleware's own thread, and here there is only the node's thread, so
// waiting is both the natural DDS idiom and the thing that drives the
// middleware (see wk_dds_node.h, "WHEN TO PUMP").

#include "wk_dds_node.h"
#include "WkMessageTypeSupportImpl.h"

#include <dds/DCPS/Marked_Default_Qos.h>

#include <cstdio>

int ACE_TMAIN(int argc, ACE_TCHAR* argv[])
{
  wk_dds::Options opt;
  opt.count = 10;
  if (!wk_dds::parse(argc, argv, opt)) {
    std::fprintf(stderr,
      "usage: dds-subscriber --peer <node-name> [--self <addr>] [--count N] [--forever]\n");
    return 1;
  }

  DDS::DomainParticipant_var dp = wk_dds::start(argc, argv, opt);
  if (!dp) return 1;

  wk::MessageTypeSupport_var ts = new wk::MessageTypeSupportImpl;
  if (ts->register_type(dp, "") != DDS::RETCODE_OK) {
    std::fprintf(stderr, "dds-subscriber: register_type failed\n");
    return 1;
  }

  CORBA::String_var type_name = ts->get_type_name();
  DDS::Topic_var topic = dp->create_topic("wk Messages", type_name,
                                          TOPIC_QOS_DEFAULT, 0,
                                          OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!topic) {
    std::fprintf(stderr, "dds-subscriber: create_topic failed\n");
    return 1;
  }

  DDS::Subscriber_var sub = dp->create_subscriber(SUBSCRIBER_QOS_DEFAULT, 0,
                                                  OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!sub) {
    std::fprintf(stderr, "dds-subscriber: create_subscriber failed\n");
    return 1;
  }

  // Must match the writer's reliability, or the two never associate — DDS
  // refuses to connect a RELIABLE reader to a BEST_EFFORT writer, and quietly
  // so. This is the most common way a first DDS program fails, and it looks
  // exactly like a broken network.
  DDS::DataReaderQos qos;
  sub->get_default_datareader_qos(qos);
  qos.reliability.kind = DDS::RELIABLE_RELIABILITY_QOS;
  qos.history.kind = DDS::KEEP_ALL_HISTORY_QOS;

  DDS::DataReader_var reader = sub->create_datareader(topic, qos, 0,
                                                      OpenDDS::DCPS::DEFAULT_STATUS_MASK);
  if (!reader) {
    std::fprintf(stderr, "dds-subscriber: create_datareader failed\n");
    return 1;
  }

  wk::MessageDataReader_var mr = wk::MessageDataReader::_narrow(reader);
  if (!mr) {
    std::fprintf(stderr, "dds-subscriber: narrow failed\n");
    return 1;
  }

  DDS::ReadCondition_var rc =
    reader->create_readcondition(DDS::ANY_SAMPLE_STATE, DDS::ANY_VIEW_STATE,
                                 DDS::ALIVE_INSTANCE_STATE);
  DDS::WaitSet_var ws = new DDS::WaitSet;
  ws->attach_condition(rc);

  std::fprintf(stderr, "dds-subscriber: waiting for samples...\n");

  int received = 0;
  while (opt.forever || received < opt.count) {
    DDS::ConditionSeq active;
    const DDS::Duration_t timeout = { 60, 0 };
    if (ws->wait(active, timeout) != DDS::RETCODE_OK) {
      std::fprintf(stderr, "dds-subscriber: wait timed out\n");
      break;
    }

    wk::MessageSeq samples;
    DDS::SampleInfoSeq info;
    if (mr->take_w_condition(samples, info, DDS::LENGTH_UNLIMITED, rc) != DDS::RETCODE_OK) {
      continue;
    }

    for (CORBA::ULong i = 0; i < samples.length(); ++i) {
      if (!info[i].valid_data) continue;   // disposals and unregisters
      std::printf("recv  #%d  id=%d from=%s  \"%s\"\n",
                  samples[i].count, samples[i].id,
                  samples[i].from.in(), samples[i].text.in());
      std::fflush(stdout);
      ++received;
    }
  }

  std::fprintf(stderr, "dds-subscriber: received %d samples\n", received);
  ws->detach_condition(rc);
  reader->delete_readcondition(rc);
  dp->delete_contained_entities();
  TheParticipantFactory->delete_participant(dp);
  TheServiceParticipant->shutdown();
  return 0;
}
