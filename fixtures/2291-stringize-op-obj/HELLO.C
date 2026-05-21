#define STR(x) #x
extern int strlen(const char *s);
int main(void) {
  static char *s = STR(hello);
  return s[0] + s[1];
}
