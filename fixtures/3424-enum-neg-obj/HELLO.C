enum Err { OK = 0, FAIL = -1, BAD = -2 };

int classify(int code) {
  if (code == FAIL) return 1;
  if (code == BAD) return 2;
  return 0;
}
