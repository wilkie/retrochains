struct Buf {
  int n;
  int data[3];
};
int main(void) {
  struct Buf b;
  b.n = 3;
  b.data[0] = 10;
  b.data[1] = 20;
  b.data[2] = 30;
  return b.n + b.data[0] + b.data[1] + b.data[2];
}
