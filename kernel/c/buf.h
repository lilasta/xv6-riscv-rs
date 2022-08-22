struct buf
{
  uchar *data;
  uint64 blockno;
  uint64 cache_index;
  void *original;
};
