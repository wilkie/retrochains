extern char *strcpy(char *dest, const char *src);
int main(void) {
  static char buf[10];
  strcpy(buf, "hello");
  return buf[0] + buf[1];
}
