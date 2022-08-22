struct buf
{
  uchar *data;
  uint32 blockno;
  uint32 deviceno;
  uint32 cache_idx;
  void *original;
};
