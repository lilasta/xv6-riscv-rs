struct buf {
  uchar data[BSIZE];
  int disk;    // does disk "own" buf?
  uint dev;
  uint blockno;
  int valid;   // has data been read from disk?
  struct sleeplock lock;
  uint refcnt;
  struct buf *prev; // LRU cache list
  struct buf *next;
};

