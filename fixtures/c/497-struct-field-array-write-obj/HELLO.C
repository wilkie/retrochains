struct buffer {
  int len;
  int data[4];
};
struct buffer b;
int g;
int main(void) {
  b.data[2] = 42;
  g = b.data[2];
  return 0;
}
