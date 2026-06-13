int main(void) {
  long a = 0x12345678L;
  int n = 4;
  long r = a << n;
  return (int)(r >> 16);
}
