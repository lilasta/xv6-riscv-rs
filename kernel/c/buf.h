struct buf
{
  uchar *data;
  uint32 blockno;
  uint64 cache_index;
  void *original;
};
