int truncate_long(long x) {
  return (int)x;
}
int main(void) {
  long y = 0x1234L;
  return truncate_long(y);
}
