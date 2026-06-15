int main(void) {
  long a = 0x12345678L;
  long b = 0x00010001L;
  long r = a + b;
  return (int)(r >> 16);
}
