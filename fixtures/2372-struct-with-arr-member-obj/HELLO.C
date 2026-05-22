struct Buf {
  int len;
  char data[4];
};
int main(void) {
  struct Buf b;
  b.len = 3;
  b.data[0] = 'A';
  b.data[1] = 'B';
  b.data[2] = 'C';
  b.data[3] = 0;
  return b.len + b.data[0];
}
