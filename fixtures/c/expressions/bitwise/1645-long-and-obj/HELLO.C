int main(void) {
  long a = 0x12345678L;
  long b = 0x0000ffffL;
  long r = a & b;
  return (int)r;
}
