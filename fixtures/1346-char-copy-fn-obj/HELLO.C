char dst[4];
void cp(char *d, char *s) {
  while (*s) *d++ = *s++;
  *d = 0;
}
int main(void) {
  cp(dst, "ab");
  return dst[1];
}
