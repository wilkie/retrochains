int my_strlen(char *s) {
  char *p = s;
  while (*p) p++;
  return p - s;
}
int main(void) {
  char *s = "ABCD";
  return my_strlen(s);
}
